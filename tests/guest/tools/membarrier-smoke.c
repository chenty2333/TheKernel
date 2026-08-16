#define _GNU_SOURCE

#if !defined(__x86_64__)
#error "membarrier smoke test requires the x86_64 Linux ABI"
#endif

#include <errno.h>
#include <linux/membarrier.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_membarrier
#define SYS_membarrier 324
#endif

#ifndef MEMBARRIER_CMD_QUERY
#define MEMBARRIER_CMD_QUERY 0
#define MEMBARRIER_CMD_GLOBAL 1
#define MEMBARRIER_CMD_GLOBAL_EXPEDITED 2
#define MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED 4
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED 8
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED 16
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE 32
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE 64
#define MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ 128
#define MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ 256
#define MEMBARRIER_CMD_GET_REGISTRATIONS 512
#define MEMBARRIER_CMD_FLAG_CPU 1
#endif

static atomic_int worker_ready;
static atomic_int worker_stop;
static atomic_int barrier_failures;

#define BARRIER_ISSUER_COUNT 4
#define BARRIER_ISSUER_ROUNDS 16

static long membarrier_call(int command, unsigned int flags, int cpu_id)
{
    return syscall(SYS_membarrier, command, flags, cpu_id);
}

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_MEMBARRIER_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_value(const char *stage, long actual, long expected)
{
    fprintf(stderr,
            "THEKERNEL_MEMBARRIER_FAIL %s actual=%ld expected=%ld errno=%d (%s)\n",
            stage, actual, expected, errno, strerror(errno));
    return 1;
}

static int expect_success(const char *stage, int command, unsigned int flags,
                          int cpu_id)
{
    errno = 0;
    long result = membarrier_call(command, flags, cpu_id);
    if (result != 0) {
        return fail(stage);
    }
    return 0;
}

static int expect_errno(const char *stage, int command, unsigned int flags,
                        int cpu_id, int expected_errno)
{
    errno = 0;
    long result = membarrier_call(command, flags, cpu_id);
    int saved_errno = errno;
    if (result != -1 || saved_errno != expected_errno) {
        errno = saved_errno;
        return fail_value(stage, result, -1);
    }
    return 0;
}

static int wait_success(pid_t child, const char *stage)
{
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail(stage);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        if (WIFSIGNALED(status)) {
            fprintf(stderr, "THEKERNEL_MEMBARRIER_FAIL %s signal=%d\n", stage,
                    WTERMSIG(status));
        } else {
            fprintf(stderr, "THEKERNEL_MEMBARRIER_FAIL %s status=0x%x\n", stage,
                    status);
        }
        return 1;
    }
    return 0;
}

static void *resident_worker(void *unused)
{
    (void)unused;
    atomic_store_explicit(&worker_ready, 1, memory_order_release);
    while (atomic_load_explicit(&worker_stop, memory_order_acquire) == 0) {
        sched_yield();
    }
    return NULL;
}

static void *barrier_issuer(void *unused)
{
    (void)unused;
    for (int round = 0; round < BARRIER_ISSUER_ROUNDS; ++round) {
        errno = 0;
        if (membarrier_call(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1) != 0) {
            atomic_fetch_add_explicit(&barrier_failures, 1, memory_order_relaxed);
        }
    }
    return NULL;
}

static int run_exec_check(int linux_host)
{
    (void)linux_host;
    /* execve creates a fresh mm, so the parent's private registration must
     * not survive into this image. */
    if (expect_errno("exec-registration-reset", MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                     0, -1, EPERM) != 0) {
        return 1;
    }
    if (expect_success("exec-register-private",
                       MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED, 0, -1) != 0 ||
        expect_success("exec-register-sync-core",
                       MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, 0,
                       -1) != 0 ||
        expect_success("exec-private", MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0,
                       -1) != 0 ||
        expect_success("exec-sync-core",
                       MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, -1) != 0) {
        return 1;
    }
    return 0;
}

static int run_smoke(const char *self, int linux_host)
{
    const long required = MEMBARRIER_CMD_PRIVATE_EXPEDITED |
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                          MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE |
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE |
                          MEMBARRIER_CMD_GET_REGISTRATIONS;
    errno = 0;
    long query = membarrier_call(MEMBARRIER_CMD_QUERY, 0, INT32_MIN);
    if (query < 0 || (query & required) != required) {
        return fail_value("query-required-mask", query, required);
    }

    if (expect_errno("query-cpu-flag", MEMBARRIER_CMD_QUERY,
                     MEMBARRIER_CMD_FLAG_CPU, -1, EINVAL) != 0 ||
        expect_errno("unknown-command", 1 << 20, 0, -1, EINVAL) != 0) {
        return 1;
    }
    if (!linux_host &&
        (expect_errno("global-not-advertised", MEMBARRIER_CMD_GLOBAL, 0, -1,
                      EINVAL) != 0 ||
         expect_errno("rseq-not-advertised",
                      MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 0, -1, EINVAL) !=
             0)) {
        return 1;
    }

    if (expect_errno("private-before-registration",
                     MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1, EPERM) != 0 ||
        expect_success("register-sync-core-only",
                       MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, 0,
                       -1) != 0 ||
        expect_errno("private-after-sync-core-only",
                     MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1, EPERM) != 0 ||
        expect_success("sync-core-after-sync-core-only",
                       MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, -1) != 0) {
        return 1;
    }
    errno = 0;
    long registrations =
        membarrier_call(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, INT32_MAX);
    if (registrations < 0 ||
        (registrations & MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE) == 0 ||
        (!linux_host &&
         (registrations & MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) != 0)) {
        /* Current upstream Linux reports only the sync-core registration here.
         * Fedora kernels observed in differential runs report both bits while
         * still returning EPERM for PRIVATE_EXPEDITED, so accept that host
         * observation without weakening TheKernel's ABI check. */
        return fail_value("get-private-registration", registrations,
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE);
    }

    if (expect_success("register-private", MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED,
                       0, -1) != 0) {
        return 1;
    }
    errno = 0;
    registrations = membarrier_call(MEMBARRIER_CMD_GET_REGISTRATIONS, 0, INT32_MAX);
    if (registrations < 0 ||
        (registrations & (MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)) !=
            (MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
             MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE)) {
        return fail_value("get-private-and-sync-core-registration", registrations,
                          MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                              MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE);
    }

    atomic_store_explicit(&worker_ready, 0, memory_order_relaxed);
    atomic_store_explicit(&worker_stop, 0, memory_order_relaxed);
    pthread_t worker;
    if (pthread_create(&worker, NULL, resident_worker, NULL) != 0) {
        return fail("pthread-create");
    }
    while (atomic_load_explicit(&worker_ready, memory_order_acquire) == 0) {
        sched_yield();
    }
    int result = expect_success("private-expedited", MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                                0, -1);

    /* Several callers must serialize behind one global request coordinator.
     * The syscall may wait for an in-flight issuer, but lock contention must
     * not escape as a spurious EAGAIN after a fixed spin count. */
    atomic_store_explicit(&barrier_failures, 0, memory_order_relaxed);
    pthread_t issuers[BARRIER_ISSUER_COUNT];
    for (int i = 0; i < BARRIER_ISSUER_COUNT; ++i) {
        if (pthread_create(&issuers[i], NULL, barrier_issuer, NULL) != 0) {
            return fail("pthread-create-barrier-issuer");
        }
    }
    for (int i = 0; i < BARRIER_ISSUER_COUNT; ++i) {
        if (pthread_join(issuers[i], NULL) != 0) {
            return fail("pthread-join-barrier-issuer");
        }
    }
    int barrier_errors =
        atomic_load_explicit(&barrier_failures, memory_order_relaxed);
    if (barrier_errors != 0) {
        return fail_value("concurrent-private-expedited", barrier_errors, 0);
    }

    atomic_store_explicit(&worker_stop, 1, memory_order_release);
    if (pthread_join(worker, NULL) != 0) {
        return fail("pthread-join");
    }
    if (result != 0) {
        return 1;
    }

    pid_t child = fork();
    if (child < 0) {
        return fail("fork");
    }
    if (child == 0) {
        _exit(membarrier_call(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1) == 0
                  ? 0
                  : 1);
    }
    if (wait_success(child, "fork-registration-inheritance") != 0) {
        return 1;
    }

    if (expect_success("register-sync-core",
                       MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE, 0,
                       -1) != 0 ||
        expect_success("private-sync-core",
                       MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE, 0, -1) != 0) {
        return 1;
    }

    child = fork();
    if (child < 0) {
        return fail("exec-fork");
    }
    if (child == 0) {
        execl(self, self, "--exec-check", linux_host ? "--linux-host" : "--thekernel",
              (char *)NULL);
        fprintf(stderr, "THEKERNEL_MEMBARRIER_FAIL exec errno=%d (%s)\n", errno,
                strerror(errno));
        _exit(127);
    }
    return wait_success(child, "exec-registration-reset");
}

int main(int argc, char **argv)
{
    int linux_host = 0;
    int exec_check = 0;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--linux-host") == 0) {
            linux_host = 1;
        } else if (strcmp(argv[i], "--thekernel") == 0) {
            linux_host = 0;
        } else if (strcmp(argv[i], "--exec-check") == 0) {
            exec_check = 1;
        } else {
            errno = EINVAL;
            return fail("arguments");
        }
    }

    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);
    if (exec_check) {
        if (run_exec_check(linux_host) != 0) {
            return 1;
        }
    } else if (run_smoke(argv[0], linux_host) != 0) {
        return 1;
    }
    puts("THEKERNEL_MEMBARRIER_SMOKE_OK");
    return 0;
}
