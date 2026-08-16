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

/* Dekker-style store-load ordering probe: two threads pinned to distinct
 * CPUs each publish a per-round value and then issue a private expedited
 * barrier before loading the peer's value. Without a real cross-CPU
 * rendezvous, store buffering lets both loads observe the previous round.
 * The barrier contract guarantees at least one side observes the fresh
 * value, so `dekker_violations` must stay zero. */
static atomic_int dekker_x;
static atomic_int dekker_y;
static atomic_int dekker_peer_ready;
#define DEKKER_ROUNDS 512
static atomic_int dekker_main_done;
static atomic_int dekker_peer_done;
static atomic_int dekker_barrier_errors;
/* Per-round staleness records so a both-stale round can be attributed
 * exactly; single-sided staleness in different rounds is not a violation. */
static unsigned char dekker_main_stale_round[DEKKER_ROUNDS];
static unsigned char dekker_peer_stale_round[DEKKER_ROUNDS];

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

static int online_cpu_count(void)
{
    cpu_set_t mask;
    CPU_ZERO(&mask);
    if (sched_getaffinity(0, sizeof(mask), &mask) != 0) {
        return -1;
    }
    return CPU_COUNT(&mask);
}

static int pin_to_cpu(int cpu)
{
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(cpu, &mask);
    return sched_setaffinity(0, sizeof(mask), &mask);
}

static void *dekker_peer(void *unused)
{
    (void)unused;
    atomic_store_explicit(&dekker_peer_ready, 1, memory_order_release);
    for (int round = 1; round <= DEKKER_ROUNDS; ++round) {
        /* Do not overrun the main thread: each round needs both stores to
         * race only inside one round window. */
        while (atomic_load_explicit(&dekker_main_done, memory_order_acquire) <
               round - 1) {
            sched_yield();
        }
        atomic_store_explicit(&dekker_y, round, memory_order_relaxed);
        if (membarrier_call(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1) != 0) {
            atomic_fetch_add_explicit(&dekker_barrier_errors, 1,
                                      memory_order_relaxed);
        }
        int seen = atomic_load_explicit(&dekker_x, memory_order_relaxed);
        /* Single-sided staleness is permitted by the barrier contract; only
         * the both-stale combination within one round is a violation. */
        dekker_peer_stale_round[round - 1] = seen < round;
        atomic_store_explicit(&dekker_peer_done, round, memory_order_release);
    }
    return NULL;
}

static int run_pinned_ordering_check(void)
{
    int cpus = online_cpu_count();
    if (cpus < 2) {
        fprintf(stderr,
                "THEKERNEL_MEMBARRIER_PINNED_SKIP online_cpus=%d\n", cpus);
        return 0;
    }

    if (pin_to_cpu(0) != 0) {
        return fail("dekker-pin-main");
    }
    pthread_t peer;
    if (pthread_create(&peer, NULL, dekker_peer, NULL) != 0) {
        return fail("dekker-pthread-create");
    }
    while (atomic_load_explicit(&dekker_peer_ready, memory_order_acquire) ==
           0) {
        sched_yield();
    }
    /* The peer inherits the creator's CPU 0 affinity; move it to CPU 1 so
     * the store-load rounds race across distinct CPUs. */
    cpu_set_t peer_mask;
    CPU_ZERO(&peer_mask);
    CPU_SET(1, &peer_mask);
    if (pthread_setaffinity_np(peer, sizeof(peer_mask), &peer_mask) != 0) {
        return fail("dekker-pin-peer");
    }

    for (int round = 1; round <= DEKKER_ROUNDS; ++round) {
        while (atomic_load_explicit(&dekker_peer_done, memory_order_acquire) <
               round - 1) {
            sched_yield();
        }
        atomic_store_explicit(&dekker_x, round, memory_order_relaxed);
        if (membarrier_call(MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, -1) != 0) {
            atomic_fetch_add_explicit(&dekker_barrier_errors, 1,
                                      memory_order_relaxed);
        }
        int seen = atomic_load_explicit(&dekker_y, memory_order_relaxed);
        dekker_main_stale_round[round - 1] = seen < round;
        atomic_store_explicit(&dekker_main_done, round, memory_order_release);
    }
    while (atomic_load_explicit(&dekker_peer_done, memory_order_acquire) <
           DEKKER_ROUNDS) {
        sched_yield();
    }
    if (pthread_join(peer, NULL) != 0) {
        return fail("dekker-pthread-join");
    }

    int errors = atomic_load_explicit(&dekker_barrier_errors,
                                      memory_order_relaxed);
    if (errors != 0) {
        return fail_value("dekker-barrier-errors", errors, 0);
    }
    int main_stale = 0;
    int peer_stale = 0;
    int both_stale = 0;
    for (int i = 0; i < DEKKER_ROUNDS; ++i) {
        main_stale += dekker_main_stale_round[i];
        peer_stale += dekker_peer_stale_round[i];
        both_stale += dekker_main_stale_round[i] && dekker_peer_stale_round[i];
    }
    if (both_stale != 0) {
        fprintf(stderr,
                "THEKERNEL_MEMBARRIER_FAIL dekker-both-stale rounds=%d\n",
                both_stale);
        return 1;
    }
    fprintf(stderr, "membarrier pinned ordering: main_stale=%d peer_stale=%d\n",
            main_stale, peer_stale);
    printf("THEKERNEL_MEMBARRIER_PINNED_ORDERING_OK rounds=%d\n",
           DEKKER_ROUNDS);
    return 0;
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

static int run_errno_matrix(int linux_host)
{
    /* Commands this kernel does not advertise must fail with EINVAL, which
     * is Linux's errno for a command outside the advertised QUERY mask.
     * Linux hosts advertise (and implement) the global/rseq commands, so
     * those assertions are TheKernel-only by design. */
    int guest_only_failed =
        !linux_host &&
        (expect_errno("matrix-global-expedited", MEMBARRIER_CMD_GLOBAL_EXPEDITED,
                      0, -1, EINVAL) != 0 ||
         expect_errno("matrix-register-global-expedited",
                      MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED, 0, -1,
                      EINVAL) != 0 ||
         expect_errno("matrix-private-rseq",
                      MEMBARRIER_CMD_PRIVATE_EXPEDITED_RSEQ, 0, -1,
                      EINVAL) != 0 ||
         expect_errno("matrix-register-private-rseq",
                      MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_RSEQ, 0, -1,
                      EINVAL) != 0 ||
         expect_errno("matrix-private-cpu-flag",
                      MEMBARRIER_CMD_PRIVATE_EXPEDITED,
                      MEMBARRIER_CMD_FLAG_CPU, 0, EINVAL) != 0);
    if (guest_only_failed) {
        return 1;
    }
    /* Both platforms must reject multi-bit command combinations, negative
     * commands, and the CPU flag on commands that never accept it. */
    if (expect_errno("matrix-combined-private-get-registrations",
                     MEMBARRIER_CMD_PRIVATE_EXPEDITED |
                         MEMBARRIER_CMD_GET_REGISTRATIONS,
                     0, -1, EINVAL) != 0 ||
        expect_errno("matrix-combined-register-private-sync-core",
                     MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED |
                         MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE,
                     0, -1, EINVAL) != 0 ||
        expect_errno("matrix-negative-command", -1, 0, -1, EINVAL) != 0 ||
        expect_errno("matrix-get-registrations-cpu-flag",
                     MEMBARRIER_CMD_GET_REGISTRATIONS,
                     MEMBARRIER_CMD_FLAG_CPU, 0, EINVAL) != 0) {
        return 1;
    }
    puts("THEKERNEL_MEMBARRIER_ERRNO_MATRIX_OK");
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
    if (run_errno_matrix(linux_host) != 0) {
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

    if (run_pinned_ordering_check() != 0) {
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
