#define _GNU_SOURCE

#if !defined(__x86_64__)
#error "userfaultfd smoke test requires the x86_64 Linux ABI"
#endif

#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#ifndef SYS_userfaultfd
#define SYS_userfaultfd 282
#endif

#ifndef UFFD_USER_MODE_ONLY
#define UFFD_USER_MODE_ONLY 1
#endif

#ifndef UFFDIO_COPY_MODE_WP
#define UFFDIO_COPY_MODE_WP (1ULL << 1)
#endif

#define TEST_PAGE_SIZE 4096UL
#define TEST_PAGE_COUNT 5UL
#define WAIT_TIMEOUT_NS 2000000000LL
#define DONTWAKE_OBSERVE_NS 50000000LL

_Static_assert(sizeof(struct uffd_msg) == 32, "bad uffd_msg ABI");
_Static_assert(sizeof(struct uffdio_api) == 24, "bad uffdio_api ABI");
_Static_assert(sizeof(struct uffdio_register) == 32,
               "bad uffdio_register ABI");
_Static_assert(sizeof(struct uffdio_copy) == 40, "bad uffdio_copy ABI");
_Static_assert(sizeof(struct uffdio_zeropage) == 32,
               "bad uffdio_zeropage ABI");
_Static_assert(offsetof(struct uffdio_zeropage, zeropage) == 24,
               "bad uffdio_zeropage output offset");
_Static_assert(offsetof(struct uffd_msg, arg.pagefault.address) == 16,
               "bad uffd_msg pagefault address offset");
_Static_assert(sizeof(void *) == 8, "executable UFFD smoke requires a 64-bit ABI");
#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "executable UFFD smoke requires the supported little-endian ABI"
#endif

/* endbr64; mov eax, 42; ret */
static const unsigned char executable_code[] = {
    0xf3, 0x0f, 0x1e, 0xfa, 0xb8, 0x2a, 0x00, 0x00, 0x00, 0xc3,
};

_Static_assert(sizeof(executable_code) <= TEST_PAGE_SIZE,
               "executable UFFD smoke code exceeds one page");

struct fault_worker {
    volatile unsigned char *address;
    unsigned char write_value;
    int write_fault;
    _Atomic int entered;
    _Atomic int completed;
    unsigned char observed;
};

struct exec_fault_worker {
    volatile unsigned char *pair_even;
    int (*entry)(void);
    _Atomic int entered;
    _Atomic int completed;
    int result;
    unsigned char pair_observed;
};

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_USERFAULTFD_FAIL %s errno=%d (%s)\n",
            stage, errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, uint64_t actual, uint64_t expected)
{
    fprintf(stderr,
            "THEKERNEL_USERFAULTFD_FAIL %s actual=%llu expected=%llu "
            "errno=%d (%s)\n",
            stage, (unsigned long long)actual, (unsigned long long)expected,
            errno, strerror(errno));
    return 1;
}

static int64_t monotonic_ns(void)
{
    struct timespec now;

    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return -1;
    }
    return (int64_t)now.tv_sec * 1000000000LL + now.tv_nsec;
}

static int wait_for_flag(_Atomic int *flag, int wanted, int64_t timeout_ns)
{
    int64_t start = monotonic_ns();

    if (start < 0) {
        return -1;
    }
    for (;;) {
        if (atomic_load_explicit(flag, memory_order_acquire) == wanted) {
            return 0;
        }
        int64_t now = monotonic_ns();
        if (now < 0) {
            return -1;
        }
        if (now - start >= timeout_ns) {
            errno = ETIMEDOUT;
            return -1;
        }
        sched_yield();
    }
}

static int require_blocked_for(_Atomic int *completed, int64_t timeout_ns)
{
    int64_t start = monotonic_ns();

    if (start < 0) {
        return -1;
    }
    for (;;) {
        if (atomic_load_explicit(completed, memory_order_acquire) != 0) {
            errno = EPROTO;
            return -1;
        }
        int64_t now = monotonic_ns();
        if (now < 0) {
            return -1;
        }
        if (now - start >= timeout_ns) {
            return 0;
        }
        sched_yield();
    }
}

static void *fault_worker_main(void *opaque)
{
    struct fault_worker *worker = opaque;

    atomic_store_explicit(&worker->entered, 1, memory_order_release);
    if (worker->write_fault) {
        *worker->address = worker->write_value;
    }
    worker->observed = *worker->address;
    atomic_store_explicit(&worker->completed, 1, memory_order_release);
    return NULL;
}

static void *exec_fault_worker_main(void *opaque)
{
    struct exec_fault_worker *worker = opaque;

    worker->pair_observed = *worker->pair_even;
    atomic_store_explicit(&worker->entered, 1, memory_order_release);
    worker->result = worker->entry();
    atomic_store_explicit(&worker->completed, 1, memory_order_release);
    return NULL;
}

static int start_fault_worker(pthread_t *thread, struct fault_worker *worker,
                              volatile unsigned char *address,
                              int write_fault, unsigned char write_value)
{
    memset(worker, 0, sizeof(*worker));
    worker->address = address;
    worker->write_fault = write_fault;
    worker->write_value = write_value;
    atomic_init(&worker->entered, 0);
    atomic_init(&worker->completed, 0);

    int result = pthread_create(thread, NULL, fault_worker_main, worker);
    if (result != 0) {
        errno = result;
        return fail("pthread-create");
    }
    if (wait_for_flag(&worker->entered, 1, WAIT_TIMEOUT_NS) != 0) {
        return fail("worker-enter-timeout");
    }
    return 0;
}

static int join_completed_worker(pthread_t thread, struct fault_worker *worker,
                                 const char *stage)
{
    if (wait_for_flag(&worker->completed, 1, WAIT_TIMEOUT_NS) != 0) {
        return fail(stage);
    }
    int result = pthread_join(thread, NULL);
    if (result != 0) {
        errno = result;
        return fail("pthread-join");
    }
    return 0;
}

static int read_fault_event(int uffd, uintptr_t expected_page,
                            int expected_write)
{
    struct pollfd poll_fd = {
        .fd = uffd,
        .events = POLLIN,
        .revents = 0,
    };
    int poll_result;

    do {
        poll_result = poll(&poll_fd, 1, 2000);
    } while (poll_result < 0 && errno == EINTR);
    if (poll_result < 0) {
        return fail("event-poll");
    }
    if (poll_result == 0) {
        errno = ETIMEDOUT;
        return fail("event-poll-timeout");
    }
    if ((poll_fd.revents & POLLIN) == 0 ||
        (poll_fd.revents & (POLLERR | POLLHUP | POLLNVAL)) != 0) {
        errno = EPROTO;
        return fail_value("event-poll-flags",
                          (uint16_t)poll_fd.revents, POLLIN);
    }

    struct uffd_msg message;
    ssize_t count;
    do {
        count = read(uffd, &message, sizeof(message));
    } while (count < 0 && errno == EINTR);
    if (count < 0) {
        return fail("event-read");
    }
    if ((size_t)count != sizeof(message)) {
        errno = EPROTO;
        return fail_value("event-read-size", (uint64_t)count,
                          sizeof(message));
    }
    if (message.event != UFFD_EVENT_PAGEFAULT) {
        errno = EPROTO;
        return fail_value("event-kind", message.event,
                          UFFD_EVENT_PAGEFAULT);
    }
    if ((uintptr_t)message.arg.pagefault.address != expected_page) {
        errno = EPROTO;
        return fail_value("event-address", message.arg.pagefault.address,
                          expected_page);
    }
    uint64_t write_flag = message.arg.pagefault.flags &
                          UFFD_PAGEFAULT_FLAG_WRITE;
    if ((write_flag != 0) != (expected_write != 0)) {
        errno = EPROTO;
        return fail_value("event-write-flag", write_flag,
                          expected_write ? UFFD_PAGEFAULT_FLAG_WRITE : 0);
    }
    return 0;
}

static int verify_copy_contents(const volatile unsigned char *destination,
                                const unsigned char *source,
                                size_t page_size, unsigned char written)
{
    if (destination[0] != written) {
        errno = EIO;
        return fail_value("copy-written-byte", destination[0], written);
    }
    for (size_t index = 1; index < page_size; ++index) {
        if (destination[index] != source[index]) {
            errno = EIO;
            return fail_value("copy-contents", destination[index],
                              source[index]);
        }
    }
    return 0;
}

static int verify_zero_contents(const volatile unsigned char *page,
                                size_t page_size)
{
    for (size_t index = 0; index < page_size; ++index) {
        if (page[index] != 0) {
            errno = EIO;
            return fail_value("zeropage-contents", page[index], 0);
        }
    }
    return 0;
}

static int test_copy(int uffd, volatile unsigned char *destination,
                     const unsigned char *source, size_t page_size)
{
    const unsigned char worker_value = 0xa5;
    struct fault_worker worker;
    pthread_t thread;

    if (start_fault_worker(&thread, &worker, destination, 1,
                           worker_value) != 0 ||
        read_fault_event(uffd, (uintptr_t)destination, 1) != 0) {
        return 1;
    }

    struct uffdio_copy copy = {
        .dst = (uint64_t)(uintptr_t)destination,
        .src = (uint64_t)(uintptr_t)source,
        .len = page_size,
        .mode = 0,
        .copy = -1,
    };
    if (ioctl(uffd, UFFDIO_COPY, &copy) != 0) {
        return fail("copy-ioctl");
    }
    if (copy.copy != (int64_t)page_size) {
        errno = EIO;
        return fail_value("copy-result", copy.copy, page_size);
    }
    if (join_completed_worker(thread, &worker, "copy-wake-timeout") != 0) {
        return 1;
    }
    if (worker.observed != worker_value) {
        errno = EIO;
        return fail_value("copy-worker-value", worker.observed,
                          worker_value);
    }
    if (verify_copy_contents(destination, source, page_size,
                             worker_value) != 0) {
        return 1;
    }
    puts("THEKERNEL_USERFAULTFD_COPY_OK");
    return 0;
}

static int test_copy_wp_error(int uffd,
                              volatile unsigned char *destination,
                              const unsigned char *source,
                              size_t page_size)
{
    struct uffdio_copy copy = {
        .dst = (uint64_t)(uintptr_t)destination,
        .src = (uint64_t)(uintptr_t)source,
        .len = page_size,
        .mode = UFFDIO_COPY_MODE_WP,
        .copy = INT64_MIN,
    };

    errno = 0;
    if (ioctl(uffd, UFFDIO_COPY, &copy) != -1 || errno != EINVAL) {
        return fail("copy-wp-ioctl");
    }
    if (copy.copy != -EINVAL) {
        errno = EIO;
        return fail_value("copy-wp-result", (uint64_t)copy.copy,
                          (uint64_t)(int64_t)-EINVAL);
    }
    puts("THEKERNEL_USERFAULTFD_COPY_WP_ERROR_OK");
    return 0;
}

static int test_zeropage(int uffd, volatile unsigned char *destination,
                         size_t page_size)
{
    struct fault_worker worker;
    pthread_t thread;

    if (start_fault_worker(&thread, &worker, destination, 0, 0) != 0 ||
        read_fault_event(uffd, (uintptr_t)destination, 0) != 0) {
        return 1;
    }

    struct uffdio_zeropage zeropage = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = page_size,
        },
        .mode = 0,
        .zeropage = -1,
    };
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &zeropage) != 0) {
        return fail("zeropage-ioctl");
    }
    if (zeropage.zeropage != (int64_t)page_size) {
        errno = EIO;
        return fail_value("zeropage-result", zeropage.zeropage,
                          page_size);
    }
    if (join_completed_worker(thread, &worker,
                              "zeropage-wake-timeout") != 0) {
        return 1;
    }
    if (worker.observed != 0) {
        errno = EIO;
        return fail_value("zeropage-worker-value", worker.observed, 0);
    }
    if (verify_zero_contents(destination, page_size) != 0) {
        return 1;
    }
    puts("THEKERNEL_USERFAULTFD_ZEROPAGE_OK");
    return 0;
}

static int test_dontwake_and_wake(int uffd,
                                  volatile unsigned char *destination,
                                  size_t page_size)
{
    struct fault_worker worker;
    pthread_t thread;

    if (start_fault_worker(&thread, &worker, destination, 0, 0) != 0 ||
        read_fault_event(uffd, (uintptr_t)destination, 0) != 0) {
        return 1;
    }

    struct uffdio_zeropage zeropage = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = page_size,
        },
        .mode = UFFDIO_ZEROPAGE_MODE_DONTWAKE,
        .zeropage = -1,
    };
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &zeropage) != 0) {
        return fail("dontwake-zeropage-ioctl");
    }
    if (zeropage.zeropage != (int64_t)page_size) {
        errno = EIO;
        return fail_value("dontwake-zeropage-result",
                          zeropage.zeropage, page_size);
    }
    if (destination[0] != 0) {
        errno = EIO;
        return fail_value("dontwake-page-visible", destination[0], 0);
    }
    if (require_blocked_for(&worker.completed, DONTWAKE_OBSERVE_NS) != 0) {
        return fail("dontwake-released-worker");
    }
    if (madvise((void *)(uintptr_t)destination, page_size,
                MADV_DONTNEED) != 0) {
        return fail("dontwake-discard");
    }
    zeropage.zeropage = -1;
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &zeropage) != 0) {
        return fail("dontwake-refill-ioctl");
    }
    if (zeropage.zeropage != (int64_t)page_size) {
        errno = EIO;
        return fail_value("dontwake-refill-result",
                          zeropage.zeropage, page_size);
    }
    if (require_blocked_for(&worker.completed, DONTWAKE_OBSERVE_NS) != 0) {
        return fail("dontwake-refill-released-worker");
    }

    struct uffdio_range wake = {
        .start = (uint64_t)(uintptr_t)destination,
        .len = page_size,
    };
    if (ioctl(uffd, UFFDIO_WAKE, &wake) != 0) {
        return fail("wake-ioctl");
    }
    if (join_completed_worker(thread, &worker, "wake-timeout") != 0) {
        return 1;
    }
    if (worker.observed != 0) {
        errno = EIO;
        return fail_value("wake-worker-value", worker.observed, 0);
    }
    puts("THEKERNEL_USERFAULTFD_DONTWAKE_WAKE_OK");
    return 0;
}

static int test_zero_progress_error(int uffd,
                                    volatile unsigned char *destination,
                                    size_t page_size)
{
    struct uffdio_zeropage zeropage = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = page_size,
        },
        .mode = 0,
        .zeropage = INT64_MIN,
    };

    errno = 0;
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &zeropage) != -1 ||
        errno != EEXIST) {
        return fail("zeropage-existing-ioctl");
    }
    if (zeropage.zeropage != -EEXIST) {
        errno = EIO;
        return fail_value("zeropage-existing-result",
                          (uint64_t)zeropage.zeropage,
                          (uint64_t)(int64_t)-EEXIST);
    }
    puts("THEKERNEL_USERFAULTFD_ERROR_OUTPUT_OK");
    return 0;
}

static int test_partial_progress(int uffd,
                                 volatile unsigned char *destination,
                                 size_t page_size,
                                 unsigned char existing_value)
{
    struct uffdio_zeropage zeropage = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = 2 * page_size,
        },
        .mode = 0,
        .zeropage = -1,
    };

    errno = 0;
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &zeropage) != -1 ||
        errno != EAGAIN) {
        return fail("zeropage-partial-ioctl");
    }
    if (zeropage.zeropage != (int64_t)page_size) {
        errno = EIO;
        return fail_value("zeropage-partial-result",
                          zeropage.zeropage, page_size);
    }
    if (verify_zero_contents(destination, page_size) != 0) {
        return 1;
    }
    if (destination[page_size] != existing_value) {
        errno = EIO;
        return fail_value("zeropage-partial-existing",
                          destination[page_size], existing_value);
    }
    puts("THEKERNEL_USERFAULTFD_PARTIAL_OK");
    return 0;
}

static int test_copyout_fault_then_wake(int uffd, size_t page_size)
{
    struct fault_worker worker;
    pthread_t thread;
    volatile unsigned char *destination = mmap(
        NULL, page_size, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (destination == MAP_FAILED) {
        return fail("copyout-target-mmap");
    }
    struct uffdio_register registration = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = page_size,
        },
        .mode = UFFDIO_REGISTER_MODE_MISSING,
        .ioctls = 0,
    };
    if (ioctl(uffd, UFFDIO_REGISTER, &registration) != 0) {
        return fail("copyout-target-register");
    }

    unsigned char *argument_pages = mmap(NULL, 2 * page_size,
                                         PROT_READ | PROT_WRITE,
                                         MAP_PRIVATE | MAP_ANONYMOUS,
                                         -1, 0);
    if (argument_pages == MAP_FAILED) {
        return fail("copyout-argument-mmap");
    }
    if (mprotect(argument_pages + page_size, page_size, PROT_NONE) != 0) {
        return fail("copyout-argument-mprotect");
    }

    const size_t input_size = offsetof(struct uffdio_zeropage, zeropage);
    struct uffdio_zeropage *zeropage =
        (struct uffdio_zeropage *)(void *)(argument_pages + page_size -
                                           input_size);
    memset(zeropage, 0, input_size);
    zeropage->range.start = (uint64_t)(uintptr_t)destination;
    zeropage->range.len = page_size;
    zeropage->mode = 0;

    if (start_fault_worker(&thread, &worker, destination, 0, 0) != 0 ||
        read_fault_event(uffd, (uintptr_t)destination, 0) != 0) {
        return 1;
    }

    errno = 0;
    if (ioctl(uffd, UFFDIO_ZEROPAGE, zeropage) != -1 ||
        errno != EFAULT) {
        return fail("copyout-fault-ioctl");
    }
    if (require_blocked_for(&worker.completed, DONTWAKE_OBSERVE_NS) != 0) {
        return fail("copyout-fault-released-worker");
    }
    if (madvise((void *)(uintptr_t)destination, page_size,
                MADV_DONTNEED) != 0) {
        return fail("copyout-fault-discard");
    }
    struct uffdio_zeropage refill = {
        .range = {
            .start = (uint64_t)(uintptr_t)destination,
            .len = page_size,
        },
        .mode = UFFDIO_ZEROPAGE_MODE_DONTWAKE,
        .zeropage = -1,
    };
    if (ioctl(uffd, UFFDIO_ZEROPAGE, &refill) != 0) {
        return fail("copyout-fault-refill");
    }
    if (refill.zeropage != (int64_t)page_size) {
        errno = EIO;
        return fail_value("copyout-fault-refill-result",
                          refill.zeropage, page_size);
    }
    if (require_blocked_for(&worker.completed, DONTWAKE_OBSERVE_NS) != 0) {
        return fail("copyout-fault-refill-released-worker");
    }

    struct uffdio_range wake = {
        .start = (uint64_t)(uintptr_t)destination,
        .len = page_size,
    };
    if (ioctl(uffd, UFFDIO_WAKE, &wake) != 0) {
        return fail("copyout-fault-wake");
    }
    if (join_completed_worker(thread, &worker,
                              "copyout-fault-wake-timeout") != 0) {
        return 1;
    }
    if (worker.observed != 0) {
        errno = EIO;
        return fail_value("copyout-fault-worker-value",
                          worker.observed, 0);
    }
    struct uffdio_range unregister = {
        .start = (uint64_t)(uintptr_t)destination,
        .len = page_size,
    };
    if (ioctl(uffd, UFFDIO_UNREGISTER, &unregister) != 0) {
        return fail("copyout-target-unregister");
    }
    if (munmap(argument_pages, 2 * page_size) != 0) {
        return fail("copyout-argument-munmap");
    }
    if (munmap((void *)(uintptr_t)destination, page_size) != 0) {
        return fail("copyout-target-munmap");
    }
    puts("THEKERNEL_USERFAULTFD_COPYOUT_FAULT_OK");
    return 0;
}

static int test_executable_copy(int uffd, size_t page_size)
{
    const size_t mapping_size = 3 * page_size;
    unsigned char *mapping = mmap(NULL, mapping_size,
                                  PROT_READ | PROT_EXEC,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        return fail("exec-target-mmap");
    }
    size_t target_index =
        (((uintptr_t)mapping / page_size) & 1U) != 0 ? 2U : 1U;
    unsigned char *target = mapping + target_index * page_size;
    volatile unsigned char *pair_even = target - page_size;
    if ((((uintptr_t)target / page_size) & 1U) == 0) {
        errno = EPROTO;
        return fail("exec-target-not-odd-page");
    }

    unsigned char *source = mmap(NULL, page_size, PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (source == MAP_FAILED) {
        return fail("exec-source-mmap");
    }
    memset(source, 0, page_size);
    memcpy(source, executable_code, sizeof(executable_code));

    struct uffdio_register registration = {
        .range = {
            .start = (uint64_t)(uintptr_t)target,
            .len = page_size,
        },
        .mode = UFFDIO_REGISTER_MODE_MISSING,
        .ioctls = 0,
    };
    if (ioctl(uffd, UFFDIO_REGISTER, &registration) != 0) {
        return fail("exec-target-register");
    }

    struct exec_fault_worker worker = {
        .pair_even = pair_even,
        .entry = (int (*)(void))(uintptr_t)target,
        .result = -1,
        .pair_observed = 0xff,
    };
    atomic_init(&worker.entered, 0);
    atomic_init(&worker.completed, 0);
    pthread_t thread;
    int result = pthread_create(&thread, NULL, exec_fault_worker_main, &worker);
    if (result != 0) {
        errno = result;
        return fail("exec-pthread-create");
    }
    if (wait_for_flag(&worker.entered, 1, WAIT_TIMEOUT_NS) != 0 ||
        read_fault_event(uffd, (uintptr_t)target, 0) != 0) {
        return fail("exec-fault-event");
    }

    struct uffdio_copy copy = {
        .dst = (uint64_t)(uintptr_t)target,
        .src = (uint64_t)(uintptr_t)source,
        .len = page_size,
        .mode = 0,
        .copy = -1,
    };
    if (ioctl(uffd, UFFDIO_COPY, &copy) != 0) {
        return fail("exec-copy-ioctl");
    }
    if (copy.copy != (int64_t)page_size) {
        errno = EIO;
        return fail_value("exec-copy-result", copy.copy, page_size);
    }
    if (wait_for_flag(&worker.completed, 1, WAIT_TIMEOUT_NS) != 0) {
        return fail("exec-worker-timeout");
    }
    result = pthread_join(thread, NULL);
    if (result != 0) {
        errno = result;
        return fail("exec-pthread-join");
    }
    if (worker.pair_observed != 0) {
        errno = EIO;
        return fail_value("exec-pair-even-value",
                          worker.pair_observed, 0);
    }
    if (worker.result != 42) {
        errno = EIO;
        return fail_value("exec-result", worker.result, 42);
    }

    struct uffdio_range unregister = {
        .start = (uint64_t)(uintptr_t)target,
        .len = page_size,
    };
    if (ioctl(uffd, UFFDIO_UNREGISTER, &unregister) != 0) {
        return fail("exec-target-unregister");
    }
    if (munmap(source, page_size) != 0 ||
        munmap(mapping, mapping_size) != 0) {
        return fail("exec-munmap");
    }
    puts("THEKERNEL_USERFAULTFD_EXEC_COPY_OK");
    return 0;
}

int main(void)
{
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    long host_page_size = sysconf(_SC_PAGESIZE);
    if (host_page_size != (long)TEST_PAGE_SIZE) {
        errno = EINVAL;
        return fail_value("page-size", (uint64_t)host_page_size,
                          TEST_PAGE_SIZE);
    }
    const size_t page_size = (size_t)host_page_size;
    const size_t mapping_size = page_size * TEST_PAGE_COUNT;

    long raw_fd = syscall(SYS_userfaultfd,
                          UFFD_USER_MODE_ONLY | O_NONBLOCK | O_CLOEXEC);
    if (raw_fd < 0) {
        return fail("create");
    }
    int uffd = (int)raw_fd;
    int descriptor_flags = fcntl(uffd, F_GETFD);
    int status_flags = fcntl(uffd, F_GETFL);
    if (descriptor_flags < 0 || status_flags < 0) {
        return fail("fcntl");
    }
    if ((descriptor_flags & FD_CLOEXEC) == 0 ||
        (status_flags & O_NONBLOCK) == 0 ||
        (status_flags & O_ACCMODE) != O_RDONLY) {
        errno = EPROTO;
        return fail("create-flags");
    }

    struct uffdio_api api = {
        .api = UFFD_API,
        .features = 0,
        .ioctls = 0,
    };
    if (ioctl(uffd, UFFDIO_API, &api) != 0) {
        return fail("api-ioctl");
    }
    const uint64_t required_api_ioctls =
        (UINT64_C(1) << _UFFDIO_API) |
        (UINT64_C(1) << _UFFDIO_REGISTER) |
        (UINT64_C(1) << _UFFDIO_UNREGISTER);
    if (api.api != UFFD_API ||
        (api.ioctls & required_api_ioctls) != required_api_ioctls) {
        errno = EPROTO;
        return fail("api-response");
    }
    puts("THEKERNEL_USERFAULTFD_API_OK");

    volatile unsigned char *mapping = mmap(
        NULL, mapping_size, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        return fail("target-mmap");
    }
    unsigned char *source = mmap(NULL, page_size,
                                 PROT_READ | PROT_WRITE,
                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (source == MAP_FAILED) {
        return fail("source-mmap");
    }
    for (size_t index = 0; index < page_size; ++index) {
        source[index] = (unsigned char)((index * 37U + 11U) & 0xffU);
    }
    const unsigned char partial_existing_value = 0x6d;
    mapping[4 * page_size] = partial_existing_value;

    struct uffdio_register registration = {
        .range = {
            .start = (uint64_t)(uintptr_t)mapping,
            .len = mapping_size,
        },
        .mode = UFFDIO_REGISTER_MODE_MISSING,
        .ioctls = 0,
    };
    if (ioctl(uffd, UFFDIO_REGISTER, &registration) != 0) {
        return fail("register-ioctl");
    }
    const uint64_t required_range_ioctls =
        (UINT64_C(1) << _UFFDIO_WAKE) |
        (UINT64_C(1) << _UFFDIO_COPY) |
        (UINT64_C(1) << _UFFDIO_ZEROPAGE);
    if ((registration.ioctls & required_range_ioctls) !=
        required_range_ioctls) {
        errno = EPROTO;
        return fail("register-response");
    }
    puts("THEKERNEL_USERFAULTFD_REGISTER_OK");

    if (test_copy_wp_error(uffd, mapping, source, page_size) != 0 ||
        test_copy(uffd, mapping, source, page_size) != 0 ||
        test_zeropage(uffd, mapping + page_size, page_size) != 0 ||
        test_dontwake_and_wake(uffd, mapping + 2 * page_size,
                               page_size) != 0 ||
        test_zero_progress_error(uffd, mapping + page_size,
                                 page_size) != 0 ||
        test_partial_progress(uffd, mapping + 3 * page_size,
                              page_size, partial_existing_value) != 0 ||
        test_copyout_fault_then_wake(uffd, page_size) != 0 ||
        test_executable_copy(uffd, page_size) != 0) {
        return 1;
    }

    struct uffdio_range unregister = {
        .start = (uint64_t)(uintptr_t)mapping,
        .len = mapping_size,
    };
    if (ioctl(uffd, UFFDIO_UNREGISTER, &unregister) != 0) {
        return fail("unregister-ioctl");
    }
    if (close(uffd) != 0) {
        return fail("close");
    }
    if (munmap((void *)(uintptr_t)source, page_size) != 0 ||
        munmap((void *)(uintptr_t)mapping, mapping_size) != 0) {
        return fail("munmap");
    }

    puts("THEKERNEL_USERFAULTFD_OK");
    return 0;
}
