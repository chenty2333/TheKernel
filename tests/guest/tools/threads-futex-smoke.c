#define _GNU_SOURCE

/* Phase-0 probe for the libc threading path that a C compiler driver or QEMU
 * (TCG) will use once it runs as an ordinary userspace process inside
 * TheKernel.
 *
 * The probe deliberately drives *libc pthreads* -- pthread_create, join,
 * futex-backed mutexes and condition variables -- because that is the path a
 * real multi-threaded workload takes, not a hand-rolled clone()/futex
 * sequence. Raw SYS_futex is used in exactly one step, to confirm that a libc
 * thread blocked in a raw futex wait is released by another libc thread, which
 * is the mechanism a libc's own mutex and condition-variable code is built on.
 *
 * Every blocking operation is bounded: each wait carries its own deadline (a
 * condvar absolute deadline, a FUTEX_WAIT timeout, a bounded poll, or a
 * bounded completion wait before every join), so a lost wakeup or a thread
 * that never runs is reported as a failure line instead of hanging the suite.
 */

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "the threads/futex smoke is x86_64-only"
#endif

/* Syscall numbers and futex operation codes are Linux x86_64 ABI values, not
 * libc-private declarations. glibc exposes them through <sys/syscall.h> and
 * <linux/futex.h>, while a musl build may not ship the latter, so the values
 * used here are defined locally behind #ifndef guards. */
#ifndef SYS_gettid
#define SYS_gettid 186
#endif
#ifndef SYS_futex
#define SYS_futex 202
#endif
#ifndef FUTEX_WAIT
#define FUTEX_WAIT 0
#endif
#ifndef FUTEX_WAKE
#define FUTEX_WAKE 1
#endif
#ifndef FUTEX_PRIVATE_FLAG
#define FUTEX_PRIVATE_FLAG 128
#endif

/* Budgets. Each one bounds a wait so that a broken kernel becomes a reported
 * failure instead of a hung test run. */
#define HANDSHAKE_ATTEMPTS 2000U         /* x 1 ms = 2 s for a peer */
#define HANDSHAKE_SLEEP_MS 1L
#define BLOCK_POLL_ATTEMPTS 5000U        /* x 1 ms = 5 s for a task to sleep */
#define BLOCK_POLL_SLEEP_MS 1L
#define CONDVAR_DEADLINE_SECONDS 10
#define CONDVAR_WAKE_ROUNDS 10U          /* x 100 ms = 1 s of retries */
#define CONDVAR_ROUND_ATTEMPTS 100U
#define FORK_WAIT_ATTEMPTS 5000U         /* x 1 ms = 5 s for the child */
#define TID_HOLD_ATTEMPTS 2000U          /* x 1 ms = 2 s of "still alive" */
#define RAW_FUTEX_DEADLINE_SECONDS 10

#define MUTEX_THREADS 4U
#define MUTEX_ITERATIONS 50000L

#define BATCH_THREADS 16U
/* A canonical user-space address pattern used only as a recognizable bit
 * pattern; the probe compares it but never dereferences it. Full 64-bit width
 * matters because a truncated thread return value would still "succeed". */
#define BATCH_RETURN_BASE UINT64_C(0x00007f5e00000000)

/* A distinctive pointer handed back by a thread's return value: pthread_join
 * must deliver exactly these bits, not a truncated or mangled variant. */
#define JOIN_RETURN_MAGIC ((void *)(uintptr_t)UINT64_C(0x00007f5eedf00d12))

#define TID_THREADS 2U
/* Completion budgets that gate a pthread_join: main only joins a thread after
 * the thread published that it is done, so "the kernel never ran the thread"
 * is reported instead of hanging inside the join. The mutex budget is sized
 * for its real workload (MUTEX_THREADS * MUTEX_ITERATIONS lock/unlock pairs)
 * and is a hang detector, not a performance bar. */
#define JOIN_ATTEMPTS 5000U              /* x 1 ms = 5 s, trivial workers */
#define MUTEX_COMPLETION_ATTEMPTS 30000U /* x 1 ms = 30 s, the counters */
#define FUTEX_COMPLETION_ATTEMPTS 15000U /* x 1 ms > the 10 s wait deadline */

/* Every failure line names the operation and the semantic condition, and
 * always carries errno plus its strerror text. pthread_* report their error as
 * a return value rather than through errno, so call sites assign errno = rc
 * before reporting; a pure userspace contract violation sets errno = EPROTO to
 * say "the observed value was wrong" as opposed to "a syscall failed". */
static int fail(const char *operation, const char *condition, long observed,
                long expected) {
    fprintf(stderr,
            "THEKERNEL_THREADS_FUTEX_FAIL %s condition=%s observed=%ld "
            "expected=%ld errno=%d (%s)\n",
            operation, condition, observed, expected, errno, strerror(errno));
    return EXIT_FAILURE;
}

static void marker(const char *value) {
    puts(value);
    fflush(stdout);
}

static void sleep_millis(long millis) {
    const struct timespec delay = {
        .tv_sec = millis / 1000L,
        .tv_nsec = (millis % 1000L) * 1000000L,
    };
    (void)nanosleep(&delay, NULL);
}

/* Bounded wait for a flag that another thread publishes. Returns 0 once the
 * flag holds `value`, 1 when the budget expired. The budget is what keeps a
 * peer that never becomes runnable from hanging the probe. */
static int wait_for_int(const _Atomic int *flag, int value,
                        unsigned int attempts) {
    for (unsigned int attempt = 0; attempt < attempts; ++attempt) {
        if (atomic_load_explicit(flag, memory_order_acquire) == value) {
            return 0;
        }
        sleep_millis(HANDSHAKE_SLEEP_MS);
    }
    return 1;
}

/* Bounded gate in front of a pthread_join. Every worker publishes its `done`
 * flag as its last action, so main reaches the join only once the thread is on
 * its way out: a thread the kernel never runs is reported here, with the step
 * name, instead of hanging the suite inside the join. Returns 0 on success and
 * the failure result otherwise. */
static int wait_for_worker(const _Atomic int *done, unsigned int attempts,
                           const char *operation) {
    if (wait_for_int(done, 1, attempts) != 0) {
        errno = ETIME;
        return fail(operation, "worker-finished-within-budget", 0, 1);
    }
    return 0;
}

static long sys_futex(uint32_t *uaddr, int operation, uint32_t value,
                      const struct timespec *timeout) {
    return syscall(SYS_futex, uaddr, operation, value, timeout, NULL, 0);
}

/* ------------------------------------------------------------------ *
 * 1. pthread_create + pthread_join with a value                      *
 * ------------------------------------------------------------------ */

static void *join_value_thread_main(void *argument) {
    _Atomic int *done = argument;

    atomic_store_explicit(done, 1, memory_order_release);
    return JOIN_RETURN_MAGIC;
}

/* Contract: a thread's return value survives pthread_join unchanged, and the
 * join orders everything the thread did before everything the joiner does
 * after. A compiler driver or QEMU hands work to helpers and reads results
 * back exactly this way; a mangled or truncated return value breaks them even
 * when thread creation itself appeared to work. */
static int test_join_value(void) {
    pthread_t thread;
    void *returned = NULL;
    _Atomic int done;

    atomic_init(&done, 0);
    errno = 0;
    int rc = pthread_create(&thread, NULL, join_value_thread_main, &done);
    if (rc != 0) {
        errno = rc;
        return fail("pthread-create-join-value", "pthread_create-rc==0", rc, 0);
    }
    /* On a completion failure the probe returns without joining: the process
     * exits immediately afterwards, which ends a thread that never ran. */
    if (wait_for_worker(&done, JOIN_ATTEMPTS, "pthread-join-value") != 0) {
        return EXIT_FAILURE;
    }
    errno = 0;
    rc = pthread_join(thread, &returned);
    if (rc != 0) {
        errno = rc;
        return fail("pthread-join-value", "pthread_join-rc==0", rc, 0);
    }
    if (returned != JOIN_RETURN_MAGIC) {
        errno = EPROTO;
        return fail("pthread-join-value", "joined-return-value",
                    (long)(intptr_t)returned,
                    (long)(intptr_t)JOIN_RETURN_MAGIC);
    }
    marker("THEKERNEL_THREADS_FUTEX_JOIN_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 2. Contended mutex over a shared counter                           *
 * ------------------------------------------------------------------ */

struct mutex_probe {
    pthread_mutex_t lock;
    /* Intentionally non-atomic: protected_total and inside are only ever
     * touched inside the critical section, and unlocked_per_thread[index] has
     * exactly one writer, so a working mutex makes every access race-free.
     * Keeping them plain is what makes a mutex that fails to exclude show up
     * as a lost update, and `overlap` catches the case where the increments
     * happen to survive anyway. */
    long protected_total;
    int inside;
    int overlap;
    int lock_error;
    int unlock_error;
    long unlocked_per_thread[MUTEX_THREADS];
};

struct mutex_worker_args {
    struct mutex_probe *probe;
    _Atomic int *done;
    unsigned int index;
};

static void *mutex_worker_main(void *argument) {
    struct mutex_worker_args *args = argument;
    struct mutex_probe *probe = args->probe;

    for (long iteration = 0; iteration < MUTEX_ITERATIONS; ++iteration) {
        int rc = pthread_mutex_lock(&probe->lock);
        if (rc != 0) {
            probe->lock_error = rc;
            break;
        }
        if (probe->inside != 0) {
            /* A second thread is inside the critical section: the mutex did
             * not exclude, whatever the counters say. */
            probe->overlap = 1;
        }
        probe->inside = 1;
        probe->protected_total += 1;
        probe->inside = 0;
        rc = pthread_mutex_unlock(&probe->lock);
        if (rc != 0) {
            probe->unlock_error = rc;
            break;
        }
        /* Deliberately unlocked: this slot has a single writer, so its final
         * value must still be exact. A shortfall means increments were lost
         * somewhere other than the shared total. */
        probe->unlocked_per_thread[args->index] += 1;
    }
    atomic_store_explicit(args->done, 1, memory_order_release);
    return NULL;
}

/* Contract: a default-attributed (futex-backed) pthread_mutex_t provides real
 * mutual exclusion under contention. This is the lock every libc internal
 * allocator, stdio stream and compiler data structure uses, so a mutex that
 * merely "works" while compiling single-threaded is not enough. */
static int test_mutex_counters(void) {
    struct mutex_probe probe;
    struct mutex_worker_args args[MUTEX_THREADS];
    _Atomic int done[MUTEX_THREADS];
    pthread_t threads[MUTEX_THREADS];
    unsigned int created = 0;

    memset(&probe, 0, sizeof(probe));
    errno = 0;
    int rc = pthread_mutex_init(&probe.lock, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("mutex-init", "pthread_mutex_init-rc==0", rc, 0);
    }

    for (unsigned int index = 0; index < MUTEX_THREADS; ++index) {
        atomic_init(&done[index], 0);
        args[index].probe = &probe;
        args[index].done = &done[index];
        args[index].index = index;
        errno = 0;
        rc = pthread_create(&threads[index], NULL, mutex_worker_main,
                            &args[index]);
        if (rc != 0) {
            errno = rc;
            for (unsigned int joined = 0; joined < created; ++joined) {
                (void)wait_for_int(&done[joined], 1,
                                   MUTEX_COMPLETION_ATTEMPTS);
                (void)pthread_join(threads[joined], NULL);
            }
            (void)pthread_mutex_destroy(&probe.lock);
            return fail("mutex-create", "pthread_create-rc==0", rc, 0);
        }
        ++created;
    }
    for (unsigned int index = 0; index < MUTEX_THREADS; ++index) {
        if (wait_for_worker(&done[index], MUTEX_COMPLETION_ATTEMPTS,
                            "mutex-worker") != 0) {
            return EXIT_FAILURE;
        }
        errno = 0;
        rc = pthread_join(threads[index], NULL);
        if (rc != 0) {
            errno = rc;
            return fail("mutex-join", "pthread_join-rc==0", rc, 0);
        }
    }

    if (probe.lock_error != 0) {
        errno = probe.lock_error;
        return fail("mutex-lock", "pthread_mutex_lock-rc==0", probe.lock_error,
                    0);
    }
    if (probe.unlock_error != 0) {
        errno = probe.unlock_error;
        return fail("mutex-unlock", "pthread_mutex_unlock-rc==0",
                    probe.unlock_error, 0);
    }
    if (probe.overlap != 0) {
        errno = EPROTO;
        return fail("mutex-exclusion", "critical-sections-never-overlap",
                    probe.overlap, 0);
    }
    const long expected_total = (long)MUTEX_THREADS * MUTEX_ITERATIONS;
    if (probe.protected_total != expected_total) {
        errno = EPROTO;
        return fail("mutex-protected-total", "total==threads*iterations",
                    probe.protected_total, expected_total);
    }
    for (unsigned int index = 0; index < MUTEX_THREADS; ++index) {
        if (probe.unlocked_per_thread[index] != MUTEX_ITERATIONS) {
            errno = EPROTO;
            return fail("mutex-unlocked-per-thread",
                        "per-thread-count==iterations",
                        probe.unlocked_per_thread[index], MUTEX_ITERATIONS);
        }
    }
    errno = 0;
    rc = pthread_mutex_destroy(&probe.lock);
    if (rc != 0) {
        errno = rc;
        return fail("mutex-destroy", "pthread_mutex_destroy-rc==0", rc, 0);
    }
    marker("THEKERNEL_THREADS_FUTEX_MUTEX_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 3. Condition-variable handshake                                    *
 * ------------------------------------------------------------------ */

struct cond_probe {
    pthread_mutex_t lock;
    pthread_cond_t cond;
    /* predicate, waiter_observed, waiter_timed_out and waiter_rc are only
     * touched while holding `lock`, or by main after a successful join. */
    int predicate;
    int waiter_observed;
    int waiter_timed_out;
    int waiter_rc;
    _Atomic int waiter_ready; /* polled by main without the lock */
    _Atomic int waiter_done;
};

static void *cond_waiter_main(void *argument) {
    struct cond_probe *probe = argument;

    int rc = pthread_mutex_lock(&probe->lock);
    if (rc != 0) {
        probe->waiter_rc = rc;
        return NULL;
    }
    if (probe->predicate == 0) {
        struct timespec deadline;
        if (clock_gettime(CLOCK_REALTIME, &deadline) != 0) {
            probe->waiter_rc = errno;
        } else {
            deadline.tv_sec += CONDVAR_DEADLINE_SECONDS;
            /* Published while holding the lock: main only needs to know that
             * the waiter is committed to the timed wait rather than taking the
             * "predicate already set" fast path. */
            atomic_store_explicit(&probe->waiter_ready, 1,
                                  memory_order_release);
            /* pthread_cond_timedwait's default clock is CLOCK_REALTIME, so the
             * absolute deadline is built from it. This deadline is the probe's
             * safety net: a lost wakeup ends the wait with ETIMEDOUT instead
             * of sleeping forever. */
            while (probe->predicate == 0) {
                rc = pthread_cond_timedwait(&probe->cond, &probe->lock,
                                            &deadline);
                if (rc == ETIMEDOUT) {
                    probe->waiter_timed_out = 1;
                    break;
                }
                if (rc != 0) {
                    probe->waiter_rc = rc;
                    break;
                }
            }
        }
    }
    if (probe->predicate != 0) {
        probe->waiter_observed = 1;
    }
    int unlock_rc = pthread_mutex_unlock(&probe->lock);
    if (unlock_rc != 0 && probe->waiter_rc == 0) {
        probe->waiter_rc = unlock_rc;
    }
    atomic_store_explicit(&probe->waiter_done, 1, memory_order_release);
    return NULL;
}

/* Contract: a thread blocked in pthread_cond_timedwait on a mutex-guarded
 * predicate is released by another thread's signal, and the predicate it then
 * observes is the one published under the lock. Compilers and QEMU use
 * condition variables for work queues; a lost wakeup there is a hang or a
 * missed job, so the wakeup is retried a bounded number of times and the
 * waiter's own absolute deadline turns a persistent loss into ETIMEDOUT. */
static int test_condvar_handshake(void) {
    struct cond_probe probe;
    pthread_t waiter;

    memset(&probe, 0, sizeof(probe));
    errno = 0;
    int rc = pthread_mutex_init(&probe.lock, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("condvar-init-mutex", "pthread_mutex_init-rc==0", rc, 0);
    }
    errno = 0;
    rc = pthread_cond_init(&probe.cond, NULL);
    if (rc != 0) {
        errno = rc;
        (void)pthread_mutex_destroy(&probe.lock);
        return fail("condvar-init-cond", "pthread_cond_init-rc==0", rc, 0);
    }

    errno = 0;
    rc = pthread_create(&waiter, NULL, cond_waiter_main, &probe);
    if (rc != 0) {
        errno = rc;
        (void)pthread_cond_destroy(&probe.cond);
        (void)pthread_mutex_destroy(&probe.lock);
        return fail("condvar-create", "pthread_create-rc==0", rc, 0);
    }

    /* Give the waiter a chance to commit to the timed wait before the wakeup
     * is published, so the probe measures the sleep/wake path rather than the
     * "predicate already set" shortcut. The wait is bounded; if it expires the
     * wakeup below still runs and the subsequent checks decide the outcome. */
    (void)wait_for_int(&probe.waiter_ready, 1, HANDSHAKE_ATTEMPTS);

    for (unsigned int round = 0; round < CONDVAR_WAKE_ROUNDS; ++round) {
        errno = 0;
        rc = pthread_mutex_lock(&probe.lock);
        if (rc != 0) {
            errno = rc;
            return fail("condvar-lock", "pthread_mutex_lock-rc==0", rc, 0);
        }
        probe.predicate = 1;
        /* The predicate is published under the lock, so the waiter either sees
         * it before sleeping or is inside the wait when the signal lands.
         * Round 0 uses the single-waiter signal path a compiler's condition
         * variable actually takes; the bounded retries broadcast, which is
         * idempotent and cannot be lost twice. */
        errno = 0;
        rc = (round == 0) ? pthread_cond_signal(&probe.cond)
                          : pthread_cond_broadcast(&probe.cond);
        int unlock_rc = pthread_mutex_unlock(&probe.lock);
        if (rc == 0) {
            rc = unlock_rc;
        }
        if (rc != 0) {
            errno = rc;
            return fail("condvar-signal", "pthread_cond_signal-rc==0", rc, 0);
        }
        if (wait_for_int(&probe.waiter_done, 1, CONDVAR_ROUND_ATTEMPTS) == 0) {
            break;
        }
    }

    /* The join is bounded by the waiter's own absolute deadline: whether it
     * saw the predicate or timed out, it unlocks and returns. */
    errno = 0;
    rc = pthread_join(waiter, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("condvar-join", "pthread_join-rc==0", rc, 0);
    }
    if (probe.waiter_rc != 0) {
        errno = probe.waiter_rc;
        return fail("condvar-wait", "pthread_cond_timedwait-rc==0",
                    probe.waiter_rc, 0);
    }
    if (probe.waiter_timed_out != 0) {
        errno = ETIMEDOUT;
        return fail("condvar-handshake", "waiter-woken-before-deadline", 0, 1);
    }
    if (probe.waiter_observed == 0) {
        errno = EPROTO;
        return fail("condvar-handshake", "waiter-observed-predicate", 0, 1);
    }
    errno = 0;
    rc = pthread_cond_destroy(&probe.cond);
    if (rc != 0) {
        errno = rc;
        return fail("condvar-destroy", "pthread_cond_destroy-rc==0", rc, 0);
    }
    errno = 0;
    rc = pthread_mutex_destroy(&probe.lock);
    if (rc != 0) {
        errno = rc;
        return fail("condvar-destroy-mutex", "pthread_mutex_destroy-rc==0", rc,
                    0);
    }
    marker("THEKERNEL_THREADS_FUTEX_CONDVAR_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 4. A batch of short-lived threads                                  *
 * ------------------------------------------------------------------ */

struct batch_args {
    _Atomic int *done;
    unsigned int index;
};

static void *batch_thread_main(void *argument) {
    struct batch_args *args = argument;

    atomic_store_explicit(args->done, 1, memory_order_release);
    return (void *)(uintptr_t)(BATCH_RETURN_BASE + args->index);
}

/* Contract: create/join/teardown works repeatedly, not once. A compiler spawns
 * and reaps many short-lived threads (and QEMU respawns worker threads), so a
 * kernel that leaks per-thread state or recycles a thread id badly shows up
 * here as a failed creation, a failed join, or a return value belonging to a
 * different thread. */
static int test_thread_batch(void) {
    pthread_t threads[BATCH_THREADS];
    _Atomic int done;

    atomic_init(&done, 0);
    for (unsigned int index = 0; index < BATCH_THREADS; ++index) {
        struct batch_args args = {.done = &done, .index = index};
        void *expected = (void *)(uintptr_t)(BATCH_RETURN_BASE + index);

        atomic_store_explicit(&done, 0, memory_order_relaxed);
        errno = 0;
        int rc = pthread_create(&threads[index], NULL, batch_thread_main,
                                &args);
        if (rc != 0) {
            /* Every earlier thread of the batch was already joined, so there
             * is nothing left to reap here; a second join would be undefined.
             */
            errno = rc;
            return fail("batch-create", "pthread_create-rc==0", rc, 0);
        }
        if (wait_for_worker(&done, JOIN_ATTEMPTS, "batch-join") != 0) {
            return EXIT_FAILURE;
        }
        void *returned = NULL;
        errno = 0;
        rc = pthread_join(threads[index], &returned);
        if (rc != 0) {
            errno = rc;
            return fail("batch-join", "pthread_join-rc==0", rc, 0);
        }
        if (returned != expected) {
            errno = EPROTO;
            return fail("batch-return-value", "thread-return-value",
                        (long)(intptr_t)returned, (long)(intptr_t)expected);
        }
    }
    marker("THEKERNEL_THREADS_FUTEX_BATCH_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 5. fork() from a non-main thread                                   *
 * ------------------------------------------------------------------ */

struct fork_probe {
    _Atomic int finished;
    int fork_errno;
    int wait_errno;
    int waited;
    int status;
};

static void *fork_worker_main(void *argument) {
    struct fork_probe *probe = argument;
    pid_t child;

    errno = 0;
    child = fork();
    if (child < 0) {
        probe->fork_errno = errno;
        goto done;
    }
    if (child == 0) {
        /* Only the forking thread exists in the child, so the child must not
         * touch state owned by the vanished threads: _exit skips stdio
         * flushing and atexit handlers that would. */
        _exit(0);
    }

    /* Bounded reap: WNOHANG polling keeps the step deadline-driven, so a child
     * that never becomes collectable is reported instead of blocking the
     * suite. */
    for (unsigned int attempt = 0; attempt < FORK_WAIT_ATTEMPTS; ++attempt) {
        int status = 0;
        errno = 0;
        pid_t reaped = waitpid(child, &status, WNOHANG);
        if (reaped == child) {
            probe->waited = 1;
            probe->status = status;
            goto done;
        }
        if (reaped < 0) {
            if (errno == EINTR) {
                continue;
            }
            probe->wait_errno = errno;
            goto done;
        }
        sleep_millis(1);
    }
    probe->wait_errno = ETIME;
    (void)kill(child, SIGKILL);
    for (unsigned int attempt = 0; attempt < HANDSHAKE_ATTEMPTS; ++attempt) {
        if (waitpid(child, NULL, WNOHANG) != 0) {
            break;
        }
        sleep_millis(1);
    }

done:
    atomic_store_explicit(&probe->finished, 1, memory_order_release);
    return NULL;
}

/* Contract: fork() from a thread of a multi-threaded process produces a child
 * that runs, exits cleanly, and is reapable by the thread that forked it.
 * Shells, make, and any process manager inside the guest fork from worker
 * threads while the rest of the process keeps running, so a kernel that
 * mishandles fork-with-threads breaks process launch long before a compiler
 * gets to run. The exact failure errno is reported for both fork and waitpid. */
static int test_fork_from_thread(void) {
    struct fork_probe probe;
    pthread_t worker;

    memset(&probe, 0, sizeof(probe));
    errno = 0;
    int rc = pthread_create(&worker, NULL, fork_worker_main, &probe);
    if (rc != 0) {
        errno = rc;
        return fail("fork-thread-create", "pthread_create-rc==0", rc, 0);
    }
    /* The main thread stays alive (blocked here) while another thread forks,
     * which is exactly the multi-threaded shape the contract covers. */
    if (wait_for_worker(&probe.finished, FORK_WAIT_ATTEMPTS + JOIN_ATTEMPTS,
                        "fork-thread-join") != 0) {
        return EXIT_FAILURE;
    }
    errno = 0;
    rc = pthread_join(worker, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("fork-thread-join", "pthread_join-rc==0", rc, 0);
    }
    if (probe.fork_errno != 0) {
        errno = probe.fork_errno;
        return fail("fork-from-thread", "fork-rc==0", -1, 0);
    }
    if (probe.waited == 0) {
        errno = probe.wait_errno != 0 ? probe.wait_errno : EPROTO;
        return fail("fork-from-thread", "child-reaped", probe.waited, 1);
    }
    if (!WIFEXITED(probe.status) || WEXITSTATUS(probe.status) != 0) {
        /* A negative observed value means the child was signalled instead of
         * exiting normally. */
        long observed = WIFEXITED(probe.status)
                            ? (long)WEXITSTATUS(probe.status)
                            : -(long)WTERMSIG(probe.status);
        errno = EPROTO;
        return fail("fork-from-thread", "child-exit-status==0", observed, 0);
    }
    marker("THEKERNEL_THREADS_FUTEX_FORK_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 6. sched_yield() and gettid() sanity                               *
 * ------------------------------------------------------------------ */

struct tid_probe {
    _Atomic pid_t tid;
    _Atomic int ready;
    _Atomic int release;
    _Atomic int exited;
};

static void *tid_worker_main(void *argument) {
    struct tid_probe *probe = argument;

    atomic_store_explicit(&probe->tid, (pid_t)syscall(SYS_gettid),
                          memory_order_release);
    atomic_store_explicit(&probe->ready, 1, memory_order_release);
    /* Stay alive until main has read both tids, so "two live threads" is a
     * concurrent observation rather than two sequential ones. Bounded, so a
     * main that never releases cannot keep the thread alive forever. */
    for (unsigned int attempt = 0; attempt < TID_HOLD_ATTEMPTS; ++attempt) {
        if (atomic_load_explicit(&probe->release, memory_order_acquire) != 0) {
            break;
        }
        sleep_millis(1);
    }
    atomic_store_explicit(&probe->exited, 1, memory_order_release);
    return NULL;
}

/* Contract: sched_yield succeeds. Runtimes and lock implementations call it to
 * give a peer a chance to run; a failure return is a scheduler contract
 * violation even though the yield itself is only a hint. */
static int test_sched_yield(void) {
    errno = 0;
    int rc = sched_yield();
    if (rc != 0) {
        return fail("sched-yield", "sched_yield-rc==0", rc, 0);
    }
    return 0;
}

/* Contract: gettid() identifies a thread -- non-zero, different between two
 * concurrently live threads, different from the main thread, and stable when
 * the same thread asks again. Runtimes key per-thread state (and /proc task
 * lookups) on this value, so duplicates or a changing value break thread
 * bookkeeping rather than just diagnostics. */
static int test_thread_identity(pid_t main_tid_start) {
    struct tid_probe probes[TID_THREADS];
    pthread_t threads[TID_THREADS];
    unsigned int created = 0;

    memset(probes, 0, sizeof(probes));
    for (unsigned int index = 0; index < TID_THREADS; ++index) {
        errno = 0;
        int rc = pthread_create(&threads[index], NULL, tid_worker_main,
                                &probes[index]);
        if (rc != 0) {
            errno = rc;
            for (unsigned int joined = 0; joined < created; ++joined) {
                atomic_store_explicit(&probes[joined].release, 1,
                                      memory_order_release);
                (void)pthread_join(threads[joined], NULL);
            }
            return fail("tid-create", "pthread_create-rc==0", rc, 0);
        }
        ++created;
    }
    for (unsigned int index = 0; index < TID_THREADS; ++index) {
        if (wait_for_int(&probes[index].ready, 1, HANDSHAKE_ATTEMPTS) != 0) {
            errno = ETIME;
            for (unsigned int other = 0; other < TID_THREADS; ++other) {
                atomic_store_explicit(&probes[other].release, 1,
                                      memory_order_release);
                (void)pthread_join(threads[other], NULL);
            }
            return fail("tid-handshake", "thread-published-gettid", 0, 1);
        }
    }

    pid_t tid_a = atomic_load_explicit(&probes[0].tid, memory_order_acquire);
    pid_t tid_b = atomic_load_explicit(&probes[1].tid, memory_order_acquire);
    int alive =
        atomic_load_explicit(&probes[0].exited, memory_order_acquire) == 0 &&
        atomic_load_explicit(&probes[1].exited, memory_order_acquire) == 0;

    for (unsigned int index = 0; index < TID_THREADS; ++index) {
        atomic_store_explicit(&probes[index].release, 1, memory_order_release);
    }
    for (unsigned int index = 0; index < TID_THREADS; ++index) {
        errno = 0;
        int rc = pthread_join(threads[index], NULL);
        if (rc != 0) {
            errno = rc;
            return fail("tid-join", "pthread_join-rc==0", rc, 0);
        }
    }

    if (!alive) {
        errno = EPROTO;
        return fail("tid-liveness", "both-threads-alive-during-read", 0, 1);
    }
    if (tid_a <= 0 || tid_b <= 0) {
        errno = EPROTO;
        return fail("gettid-worker", "thread-gettid-nonzero",
                    (long)(tid_a <= 0 ? tid_a : tid_b), 1);
    }
    if (tid_a == tid_b) {
        errno = EPROTO;
        return fail("gettid-distinct", "live-thread-tids-differ", (long)tid_b,
                    (long)tid_a);
    }
    if (tid_a == main_tid_start || tid_b == main_tid_start) {
        errno = EPROTO;
        return fail("gettid-distinct", "worker-tid-differs-from-main",
                    (long)(tid_a == main_tid_start ? tid_a : tid_b),
                    (long)main_tid_start);
    }
    errno = 0;
    pid_t main_tid_end = (pid_t)syscall(SYS_gettid);
    if (main_tid_end <= 0) {
        return fail("gettid-main-end", "main-gettid-nonzero",
                    (long)main_tid_end, 1);
    }
    if (main_tid_end != main_tid_start) {
        errno = EPROTO;
        return fail("gettid-main-stable", "same-thread-same-tid",
                    (long)main_tid_end, (long)main_tid_start);
    }
    marker("THEKERNEL_THREADS_FUTEX_TID_OK");
    return 0;
}

/* ------------------------------------------------------------------ *
 * 7. Raw futex wait/wake round trip between two libc threads         *
 * ------------------------------------------------------------------ */

struct raw_futex_probe {
    uint32_t *word;
    _Atomic pid_t waiter_tid;
    _Atomic int waiter_finished;
    _Atomic int waker_finished;
    long wait_result;
    int wait_errno;
    uint32_t wait_observed_word;
    long wake_result;
    int wake_errno;
    int handshake_errno;
};

/* Read the scheduler-state character of a task from /proc/<pid>/task/<tid>/stat.
 * The comm field may contain spaces and parentheses, so parse from the last
 * ')'. This is the minimal form of the block handshake that
 * tests/guest/portable/futex-smoke.c uses. */
static int task_state(pid_t pid, pid_t tid) {
    char path[96];
    char buffer[512];

    int length = snprintf(path, sizeof(path), "/proc/%ld/task/%ld/stat",
                          (long)pid, (long)tid);
    if (length <= 0 || (size_t)length >= sizeof(path)) {
        errno = ENAMETOOLONG;
        return -1;
    }
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    (void)close(fd);
    if (count <= 0) {
        if (count == 0) {
            errno = EIO;
        }
        return -1;
    }
    buffer[count] = '\0';
    const char *close_paren = strrchr(buffer, ')');
    if (close_paren == NULL || close_paren[1] != ' ' ||
        close_paren[2] == '\0') {
        errno = EPROTO;
        return -1;
    }
    return (unsigned char)close_paren[2];
}

/* A task shows state 'S' only once it has slept, and FUTEX_WAIT sleeps
 * strictly after the waiter is queued on the futex, so polling until 'S'
 * proves the waiter is queued. That makes a wake count of zero a real lost
 * wake rather than a race with thread startup -- which matters most under an
 * emulated guest, where timing-based handshakes are unreliable. The poll is
 * bounded, so a kernel that never sleeps the waiter is reported, not hung. */
static int wait_until_task_blocked(pid_t pid, pid_t tid) {
    for (unsigned int attempt = 0; attempt < BLOCK_POLL_ATTEMPTS; ++attempt) {
        int state = task_state(pid, tid);
        if (state == 'S') {
            return 0;
        }
        if (state < 0) {
            return -1;
        }
        sleep_millis(BLOCK_POLL_SLEEP_MS);
    }
    errno = ETIME;
    return -1;
}

static void *raw_futex_waiter_main(void *argument) {
    struct raw_futex_probe *probe = argument;
    const struct timespec timeout = {
        .tv_sec = RAW_FUTEX_DEADLINE_SECONDS,
        .tv_nsec = 0,
    };

    atomic_store_explicit(&probe->waiter_tid, (pid_t)syscall(SYS_gettid),
                          memory_order_release);
    errno = 0;
    probe->wait_result = sys_futex(probe->word, FUTEX_WAIT | FUTEX_PRIVATE_FLAG,
                                   0, &timeout);
    probe->wait_errno = errno;
    probe->wait_observed_word =
        __atomic_load_n(probe->word, __ATOMIC_ACQUIRE);
    atomic_store_explicit(&probe->waiter_finished, 1, memory_order_release);
    return NULL;
}

static void *raw_futex_waker_main(void *argument) {
    struct raw_futex_probe *probe = argument;
    pid_t tid = 0;

    for (unsigned int attempt = 0; attempt < HANDSHAKE_ATTEMPTS; ++attempt) {
        tid = atomic_load_explicit(&probe->waiter_tid, memory_order_acquire);
        if (tid > 0) {
            break;
        }
        sleep_millis(HANDSHAKE_SLEEP_MS);
    }
    if (tid <= 0) {
        probe->handshake_errno = ETIME;
        goto done;
    }
    if (wait_until_task_blocked(getpid(), tid) != 0) {
        probe->handshake_errno = errno != 0 ? errno : ETIME;
        /* Best effort: release the waiter anyway so it does not have to sit
         * out its full deadline before main can report the handshake failure. */
        __atomic_store_n(probe->word, 1, __ATOMIC_RELEASE);
        (void)sys_futex(probe->word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL);
        goto done;
    }

    __atomic_store_n(probe->word, 1, __ATOMIC_RELEASE);
    errno = 0;
    probe->wake_result = sys_futex(probe->word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG,
                                   1, NULL);
    probe->wake_errno = errno;

done:
    atomic_store_explicit(&probe->waker_finished, 1, memory_order_release);
    return NULL;
}

/* Contract: a libc thread blocked in a raw FUTEX_WAIT on a private word is
 * woken by another libc thread's FUTEX_WAKE, observes the releasing store, and
 * the wake reports exactly one waiter.
 *
 * tests/guest/portable/futex-smoke.c already owns the differential coverage of
 * the futex ABI itself (EAGAIN and ETIMEDOUT boundaries, wake accounting,
 * requeue, bitsets, private/shared scope, shared-file aliasing), so this is
 * deliberately the minimal shape: one word, one waiter, one waker. What it
 * adds is the combination that helper does not exercise -- libc-created
 * threads entering and leaving a raw futex wait -- which is the mechanism a
 * libc mutex or condition variable is built on. */
static int test_raw_futex_roundtrip(void) {
    static uint32_t word;
    struct raw_futex_probe probe;
    pthread_t waiter;
    pthread_t waker;

    memset(&probe, 0, sizeof(probe));
    probe.word = &word;
    word = 0;

    errno = 0;
    int rc = pthread_create(&waiter, NULL, raw_futex_waiter_main, &probe);
    if (rc != 0) {
        errno = rc;
        return fail("raw-futex-waiter-create", "pthread_create-rc==0", rc, 0);
    }
    errno = 0;
    rc = pthread_create(&waker, NULL, raw_futex_waker_main, &probe);
    if (rc != 0) {
        errno = rc;
        __atomic_store_n(&word, 1, __ATOMIC_RELEASE);
        (void)sys_futex(&word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, NULL);
        (void)pthread_join(waiter, NULL);
        return fail("raw-futex-waker-create", "pthread_create-rc==0", rc, 0);
    }

    /* Both joins are bounded: the waker's handshake polls are bounded and the
     * waiter's FUTEX_WAIT carries its own timeout, so both threads publish
     * completion even when the wakeup is lost. */
    if (wait_for_worker(&probe.waker_finished, FUTEX_COMPLETION_ATTEMPTS,
                        "raw-futex-waker-join") != 0) {
        return EXIT_FAILURE;
    }
    errno = 0;
    rc = pthread_join(waker, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("raw-futex-waker-join", "pthread_join-rc==0", rc, 0);
    }
    if (wait_for_worker(&probe.waiter_finished, FUTEX_COMPLETION_ATTEMPTS,
                        "raw-futex-waiter-join") != 0) {
        return EXIT_FAILURE;
    }
    errno = 0;
    rc = pthread_join(waiter, NULL);
    if (rc != 0) {
        errno = rc;
        return fail("raw-futex-waiter-join", "pthread_join-rc==0", rc, 0);
    }

    if (probe.handshake_errno != 0) {
        errno = probe.handshake_errno;
        return fail("raw-futex-block-handshake", "waiter-queued-before-wake", 0,
                    1);
    }
    if (probe.wait_result != 0) {
        errno = probe.wait_errno != 0 ? probe.wait_errno : EPROTO;
        return fail("raw-futex-wait", "FUTEX_WAIT-rc==0 (woken, not timed out)",
                    probe.wait_result, 0);
    }
    if (probe.wake_result != 1) {
        errno = probe.wake_errno != 0 ? probe.wake_errno : EPROTO;
        return fail("raw-futex-wake", "FUTEX_WAKE-woke-one-waiter",
                    probe.wake_result, 1);
    }
    if (probe.wait_observed_word != 1) {
        errno = EPROTO;
        return fail("raw-futex-word", "waiter-observed-released-word",
                    probe.wait_observed_word, 1);
    }
    marker("THEKERNEL_THREADS_FUTEX_RAW_FUTEX_OK");
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    /* Thread identity is captured before anything else so that the "the same
     * thread still has the same tid" check covers the whole probe. */
    errno = 0;
    pid_t main_tid_start = (pid_t)syscall(SYS_gettid);
    if (main_tid_start <= 0) {
        return fail("gettid-main-start", "main-gettid-nonzero",
                    (long)main_tid_start, 1);
    }

    if (test_join_value() != 0) {
        return EXIT_FAILURE;
    }
    if (test_mutex_counters() != 0) {
        return EXIT_FAILURE;
    }
    if (test_condvar_handshake() != 0) {
        return EXIT_FAILURE;
    }
    if (test_thread_batch() != 0) {
        return EXIT_FAILURE;
    }
    if (test_fork_from_thread() != 0) {
        return EXIT_FAILURE;
    }
    if (test_sched_yield() != 0) {
        return EXIT_FAILURE;
    }
    if (test_thread_identity(main_tid_start) != 0) {
        return EXIT_FAILURE;
    }
    if (test_raw_futex_roundtrip() != 0) {
        return EXIT_FAILURE;
    }

    puts("THEKERNEL_THREADS_FUTEX_OK");
    fflush(stdout);
    return EXIT_SUCCESS;
}
