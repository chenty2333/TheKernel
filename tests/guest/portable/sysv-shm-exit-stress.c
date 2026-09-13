/*
 * Stress the SysV SHM exit-retirement window that made the `sysv-shm` guest
 * case flaky.
 *
 * The acceptance case (tests/guest/system-init.c::test_sysv_shm) runs the
 * window once per boot: a parent attaches to a segment, marks it IPC_RMID,
 * forks a child that inherits those attachments, waits for the child, detaches
 * its own attachments, and then requires `shmat(id)` to fail with EINVAL. That
 * final shmat is only correct if the child's *inherited* attachment records
 * died with the child. If they are retired by a deferred VMA finalizer that
 * runs after the zombie is published, the parent's `wait()` can return while
 * the removed segment is still attachable, and the assertion fails with
 * errno==0.
 *
 * This program loops that topology (and variants of it) many times per boot so
 * the window can be measured at the invocation level instead of the boot
 * level.
 *
 * Variants:
 *   wait   - the acceptance topology exactly: one child, parent waits for it,
 *            then detaches and probes. This is the primary attack.
 *   pipe   - no wait() at all. The parent blocks until EOF on a pipe whose
 *            write end is held only by the child; the kernel's final-exit path
 *            closes the fd table (kernel/src/task/ops.rs) after retiring IPC
 *            state and before publishing the zombie, so "EOF observed" implies
 *            "the child's SHM attachments are already retired" in any correct
 *            ordering. The probe that follows is therefore a hard assertion
 *            that does not depend on wait(), and it fails deterministically on
 *            an exit path that retires attachments later.
 *   multi  - four children attached and exited before the parent waits for all
 *            of them; any single child's surviving record keeps the removed
 *            segment reachable, so this presses on the same window four times
 *            per round.
 *   nowait - the parent probes with no synchronisation at all right after
 *            fork(), then waits and probes again. The unsynchronised probe is
 *            descriptive only (a child that has not exited legitimately still
 *            owns the segment); only the post-reap probe is an assertion.
 *   shared - negative control for over-retirement: a child exits while a
 *            grandchild still holds the segment, so the removed segment must
 *            stay reachable until the grandchild is gone. It catches a fix
 *            that retires more than the exiting process's own attachments.
 *            A two-generation round has more ways to be cut short than a
 *            one-generation round, so both the grandchild's failure path and
 *            the child's incomplete read report a code and an errno through
 *            the `finished` pipe the parent already reads. That turns "the
 *            child exited 5" into "the grandchild's shmat failed with EINVAL"
 *            or "the grandchild never reported", which is the difference
 *            between a kernel finding and an unattributed flake.
 *
 * Every round creates its own private segment, so rounds are independent.
 * Output is one machine-readable summary line per invocation; the process exit
 * status is 0 only when no assertion failed.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/shm.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

enum {
    SEGMENT_BYTES = 4096,
    PARENT_VALUE = 17,
    CHILD_VALUE = 23,
    GRANDCHILD_OFFSET = 8,
    GRANDCHILD_VALUE = 0x5a,
    MULTI_CHILDREN = 4,
    MULTI_STRIDE = 64,
};

/*
 * Why a `shared` round stopped early.  The process that notices writes one
 * report to the `finished` pipe before exiting, so the parent can name the
 * failing syscall instead of only observing that the child exited 5.
 */
enum {
    GRANDCHILD_SHMAT = 1,
    GRANDCHILD_BAD_VALUE = 2,
    GRANDCHILD_WRITE = 3,
    GRANDCHILD_DETACH = 4,
    GRANDCHILD_TOKEN = 5,
    GRANDCHILD_FORK = 6,
    CHILD_READ_INCOMPLETE = 7,
};

struct failure_report {
    int code;
    int error;
};

struct counters {
    unsigned long rounds;
    unsigned long completed;
    unsigned long setup_fail;
    unsigned long child_fail;
    unsigned long value_fail;
    unsigned long attach_fail;
    unsigned long errno_fail;
    unsigned long retire_early_fail;
    unsigned long grandchild_fail;
    unsigned long alive_probe;
    unsigned long polls_max;
    unsigned long polls_sum;
};

struct first_fail {
    int seen;
    unsigned long round;
    const char *kind;
    int error;
    unsigned long data;
};

static void record_failure(struct first_fail *first, unsigned long round,
                           const char *kind, int error, unsigned long data)
{
    if (first->seen)
        return;
    first->seen = 1;
    first->round = round;
    first->kind = kind;
    first->error = error;
    first->data = data;
}

/*
 * Best-effort failure report from a dying grandchild or child.  A short write
 * is ignored: the report is a diagnostic, and the round is already failing.
 */
static void report_failure(int fd, int code, int error)
{
    struct failure_report report;
    report.code = code;
    report.error = error;
    ssize_t ignored = write(fd, &report, sizeof(report));
    (void)ignored;
}

static int read_failure_report(int fd, struct failure_report *report)
{
    ssize_t got;
    do {
        got = read(fd, report, sizeof(*report));
    } while (got < 0 && errno == EINTR);
    return got == (ssize_t)sizeof(*report) ? 1 : 0;
}

static int reap(pid_t pid, int *status)
{
    pid_t reaped;
    do {
        reaped = waitpid(pid, status, 0);
    } while (reaped < 0 && errno == EINTR);
    return reaped == pid ? 0 : -1;
}

/*
 * Build the acceptance topology's segment: private, 4096 bytes, marked
 * IPC_RMID with the parent holding one read-write and one read-only
 * attachment. Returns 0 and fills the outputs, or -1 with errno from the
 * failing call.
 */
static int open_round(int *id, unsigned char **first, unsigned char **second)
{
    errno = 0;
    int shmid = shmget(IPC_PRIVATE, SEGMENT_BYTES, IPC_CREAT | 0600);
    if (shmid < 0)
        return -1;
    errno = 0;
    unsigned char *read_write = shmat(shmid, NULL, 0);
    if (read_write == (void *)-1) {
        (void)shmctl(shmid, IPC_RMID, NULL);
        return -1;
    }
    read_write[0] = PARENT_VALUE;
    errno = 0;
    if (shmctl(shmid, IPC_RMID, NULL) != 0) {
        (void)shmdt(read_write);
        return -1;
    }
    errno = 0;
    unsigned char *read_only = shmat(shmid, NULL, SHM_RDONLY);
    if (read_only == (void *)-1) {
        (void)shmdt(read_write);
        return -1;
    }
    *id = shmid;
    *first = read_write;
    *second = read_only;
    return 0;
}

/*
 * The assertion the whole flake was about: a segment marked IPC_RMID with no
 * attachments left must refuse shmat with EINVAL. Returns 0 when it does.
 * A returned address is detached again so the segment cannot be kept alive by
 * the probe itself, and is reported through `observed`.
 */
static int probe_removed(int id, void **observed, int *observed_errno)
{
    errno = 0;
    void *addr = shmat(id, NULL, 0);
    if (addr == (void *)-1) {
        *observed = NULL;
        *observed_errno = errno;
        return errno == EINVAL ? 0 : -1;
    }
    *observed = addr;
    *observed_errno = 0;
    (void)shmdt(addr);
    return -1;
}

static void check_probe(int id, unsigned long round, struct counters *counters,
                        struct first_fail *first)
{
    void *observed = NULL;
    int observed_errno = 0;
    counters->completed++;
    if (probe_removed(id, &observed, &observed_errno) == 0)
        return;
    if (observed != NULL) {
        counters->attach_fail++;
        record_failure(first, round, "removed-id-still-attachable", 0,
                       (unsigned long)(uintptr_t)observed);
    } else {
        counters->errno_fail++;
        record_failure(first, round, "removed-id-wrong-errno",
                       observed_errno, 0);
    }
}

/* Child body shared by every single-child variant: attach once more, observe
 * the parent's value, publish its own, detach that explicit attachment, and
 * exit with the two inherited attachments still owned. */
static void child_attach_and_exit(int id)
{
    errno = 0;
    unsigned char *own = shmat(id, NULL, 0);
    if (own == (void *)-1 || own[0] != PARENT_VALUE)
        _exit(1);
    own[0] = CHILD_VALUE;
    if (shmdt(own) != 0)
        _exit(2);
    _exit(0);
}

static void round_wait(unsigned long round, struct counters *counters,
                       struct first_fail *first)
{
    int id = -1;
    unsigned char *rw = NULL;
    unsigned char *ro = NULL;
    if (open_round(&id, &rw, &ro) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "setup", errno, 0);
        return;
    }
    errno = 0;
    pid_t child = fork();
    if (child < 0) {
        (void)shmdt(ro);
        (void)shmdt(rw);
        counters->setup_fail++;
        record_failure(first, round, "fork", errno, 0);
        return;
    }
    if (child == 0)
        child_attach_and_exit(id);
    int status = -1;
    if (reap(child, &status) != 0 || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        counters->child_fail++;
        record_failure(first, round, "child-status", status, 0);
    }
    if (rw[0] != CHILD_VALUE || ro[0] != CHILD_VALUE) {
        counters->value_fail++;
        record_failure(first, round, "shared-page-value", rw[0], ro[0]);
    }
    (void)shmdt(ro);
    (void)shmdt(rw);
    check_probe(id, round, counters, first);
}

static void round_pipe(unsigned long round, struct counters *counters,
                       struct first_fail *first)
{
    int id = -1;
    unsigned char *rw = NULL;
    unsigned char *ro = NULL;
    if (open_round(&id, &rw, &ro) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "setup", errno, 0);
        return;
    }
    int eof_pipe[2];
    errno = 0;
    if (pipe(eof_pipe) != 0) {
        (void)shmdt(ro);
        (void)shmdt(rw);
        counters->setup_fail++;
        record_failure(first, round, "pipe", errno, 0);
        return;
    }
    errno = 0;
    pid_t child = fork();
    if (child < 0) {
        close(eof_pipe[0]);
        close(eof_pipe[1]);
        (void)shmdt(ro);
        (void)shmdt(rw);
        counters->setup_fail++;
        record_failure(first, round, "fork", errno, 0);
        return;
    }
    if (child == 0) {
        close(eof_pipe[0]);
        /* Deliberately keep eof_pipe[1] open across _exit(): its close is the
         * parent's clock for "this process's fd table is being torn down". */
        child_attach_and_exit(id);
    }
    close(eof_pipe[1]);
    (void)shmdt(ro);
    (void)shmdt(rw);
    char token = 0;
    ssize_t got;
    do {
        got = read(eof_pipe[0], &token, 1);
    } while (got < 0 && errno == EINTR);
    close(eof_pipe[0]);
    if (got != 0) {
        counters->setup_fail++;
        record_failure(first, round, "child-eof", (int)got, 0);
    }
    check_probe(id, round, counters, first);
    int status = -1;
    if (reap(child, &status) != 0 || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        counters->child_fail++;
        record_failure(first, round, "child-status", status, 0);
    }
}

static void round_multi(unsigned long round, struct counters *counters,
                        struct first_fail *first)
{
    int id = -1;
    unsigned char *rw = NULL;
    unsigned char *ro = NULL;
    if (open_round(&id, &rw, &ro) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "setup", errno, 0);
        return;
    }
    pid_t children[MULTI_CHILDREN];
    unsigned started = 0;
    int failed = 0;
    for (; started < MULTI_CHILDREN; ++started) {
        errno = 0;
        pid_t child = fork();
        if (child < 0) {
            failed = 1;
            counters->setup_fail++;
            record_failure(first, round, "fork", errno, 0);
            break;
        }
        if (child == 0) {
            unsigned slot = started;
            errno = 0;
            unsigned char *own = shmat(id, NULL, 0);
            if (own == (void *)-1 || own[0] != PARENT_VALUE)
                _exit(1);
            own[(slot + 1) * MULTI_STRIDE] = (unsigned char)(CHILD_VALUE + slot);
            _exit(shmdt(own) != 0);
        }
        children[started] = child;
    }
    for (unsigned i = 0; i < started; ++i) {
        int status = -1;
        if (reap(children[i], &status) != 0 || !WIFEXITED(status) ||
            WEXITSTATUS(status) != 0) {
            counters->child_fail++;
            record_failure(first, round, "child-status", status, i);
            failed = 1;
        }
    }
    for (unsigned i = 0; i < started; ++i) {
        if (rw[(i + 1) * MULTI_STRIDE] != (unsigned char)(CHILD_VALUE + i)) {
            counters->value_fail++;
            record_failure(first, round, "shared-page-value", rw[(i + 1) * MULTI_STRIDE], i);
            failed = 1;
        }
    }
    (void)failed;
    (void)shmdt(ro);
    (void)shmdt(rw);
    check_probe(id, round, counters, first);
}

static void round_nowait(unsigned long round, struct counters *counters,
                         struct first_fail *first)
{
    int id = -1;
    unsigned char *rw = NULL;
    unsigned char *ro = NULL;
    if (open_round(&id, &rw, &ro) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "setup", errno, 0);
        return;
    }
    errno = 0;
    pid_t child = fork();
    if (child < 0) {
        (void)shmdt(ro);
        (void)shmdt(rw);
        counters->setup_fail++;
        record_failure(first, round, "fork", errno, 0);
        return;
    }
    if (child == 0)
        child_attach_and_exit(id);
    (void)shmdt(ro);
    (void)shmdt(rw);
    /* Descriptive, not an assertion: a child that has not got as far as
     * exiting legitimately still owns the segment. */
    void *observed = NULL;
    int observed_errno = 0;
    int alive = probe_removed(id, &observed, &observed_errno) != 0 &&
                observed != NULL;
    if (alive)
        counters->alive_probe++;
    int status = -1;
    if (reap(child, &status) != 0 || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        counters->child_fail++;
        record_failure(first, round, "child-status", status, 0);
    }
    check_probe(id, round, counters, first);
}

/*
 * A child exits while a grandchild still holds the segment. The removed
 * segment must stay reachable until the grandchild is gone, so this catches a
 * retirement that is too eager (for example one that clears every attachment
 * in the namespace rather than the exiting process's own).
 */
static void round_shared(unsigned long round, struct counters *counters,
                         struct first_fail *first)
{
    int id = -1;
    unsigned char *rw = NULL;
    unsigned char *ro = NULL;
    if (open_round(&id, &rw, &ro) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "setup", errno, 0);
        return;
    }
    int attached[2];
    int release[2];
    int finished[2];
    if (pipe(attached) != 0 || pipe(release) != 0 || pipe(finished) != 0) {
        counters->setup_fail++;
        record_failure(first, round, "pipe", errno, 0);
        (void)shmdt(ro);
        (void)shmdt(rw);
        return;
    }
    errno = 0;
    pid_t child = fork();
    if (child < 0) {
        counters->setup_fail++;
        record_failure(first, round, "fork", errno, 0);
        (void)shmdt(ro);
        (void)shmdt(rw);
        return;
    }
    if (child == 0) {
        close(release[1]);
        close(finished[0]);
        errno = 0;
        pid_t grandchild = fork();
        if (grandchild < 0) {
            report_failure(finished[1], GRANDCHILD_FORK, errno);
            _exit(4);
        }
        if (grandchild == 0) {
            close(finished[0]);
            errno = 0;
            unsigned char *shared = shmat(id, NULL, 0);
            if (shared == (void *)-1) {
                int error = errno;
                report_failure(finished[1], GRANDCHILD_SHMAT, error);
                _exit(1);
            }
            if (shared[0] != PARENT_VALUE) {
                report_failure(finished[1], GRANDCHILD_BAD_VALUE, shared[0]);
                _exit(1);
            }
            shared[GRANDCHILD_OFFSET] = GRANDCHILD_VALUE;
            errno = 0;
            if (write(attached[1], "a", 1) != 1) {
                int error = errno;
                report_failure(finished[1], GRANDCHILD_WRITE, error);
                _exit(2);
            }
            close(attached[1]);
            char token;
            ssize_t got;
            do {
                got = read(release[0], &token, 1);
            } while (got < 0 && errno == EINTR);
            /* EOF on the release pipe is the parent's permission to go. */
            errno = 0;
            if (shmdt(shared) != 0) {
                int error = errno;
                report_failure(finished[1], GRANDCHILD_DETACH, error);
                _exit(3);
            }
            if (got > 0) {
                report_failure(finished[1], GRANDCHILD_TOKEN, (int)got);
                _exit(3);
            }
            _exit(0);
        }
        close(attached[1]);
        close(release[0]);
        close(finished[1]);
        char token;
        ssize_t got;
        do {
            got = read(attached[0], &token, 1);
        } while (got < 0 && errno == EINTR);
        /* Exit while the grandchild still owns the segment. */
        if (got != 1) {
            int error = errno;
            report_failure(finished[1], CHILD_READ_INCOMPLETE,
                           got == 0 ? 0 : error);
            _exit(5);
        }
        _exit(0);
    }
    close(attached[0]);
    close(attached[1]);
    close(release[0]);
    close(finished[1]);
    /* Drop the parent's own attachments immediately: from here the segment is
     * kept alive only by the child's and grandchild's inherited records, which
     * is what makes the post-child-exit probe below a real assertion. */
    (void)shmdt(ro);
    (void)shmdt(rw);
    int status = -1;
    int child_failed =
        reap(child, &status) != 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 0;
    /* The child is gone but the grandchild still holds the segment: the
     * removed id must still be attachable, and the grandchild's write must be
     * visible through the new mapping. */
    errno = 0;
    unsigned char *probe = shmat(id, NULL, 0);
    if (probe == (void *)-1) {
        counters->retire_early_fail++;
        record_failure(first, round, "live-attachment-not-reachable", errno, 0);
    } else {
        if (probe[GRANDCHILD_OFFSET] != GRANDCHILD_VALUE) {
            counters->value_fail++;
            record_failure(first, round, "shared-page-value",
                           probe[GRANDCHILD_OFFSET], 0);
        }
        (void)shmdt(probe);
    }
    close(release[1]);
    /* The grandchild writes here only when it could not do its part; the
     * child writes here only when its read did not complete. A clean round
     * reads nothing at all (EOF), which is what the old code asserted. */
    struct failure_report report;
    memset(&report, 0, sizeof(report));
    int reported = read_failure_report(finished[0], &report);
    close(finished[0]);
    if (!reported && child_failed) {
        counters->child_fail++;
        record_failure(first, round, "child-status", status, 0);
    } else if (reported && report.code == CHILD_READ_INCOMPLETE) {
        counters->child_fail++;
        record_failure(first, round, "child-read-incomplete", report.error, 0);
    } else if (reported) {
        counters->grandchild_fail++;
        record_failure(first, round, "grandchild-report", report.code,
                       (unsigned long)report.error);
    }
    check_probe(id, round, counters, first);
}

typedef void (*round_fn)(unsigned long, struct counters *, struct first_fail *);

static const struct {
    const char *name;
    round_fn run;
} VARIANTS[] = {
    {"wait", round_wait},
    {"pipe", round_pipe},
    {"multi", round_multi},
    {"nowait", round_nowait},
    {"shared", round_shared},
};

static void on_alarm(int signo)
{
    (void)signo;
    static const char message[] = "SYSV-SHM-STRESS-TIMEOUT\n";
    ssize_t ignored = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)ignored;
    _exit(124);
}

int main(int argc, char **argv)
{
    const char *variant = argc > 1 ? argv[1] : "wait";
    unsigned long rounds = argc > 2 ? strtoul(argv[2], NULL, 10) : 500;
    unsigned alarm_seconds = argc > 3 ? (unsigned)strtoul(argv[3], NULL, 10) : 120;
    round_fn run = NULL;
    for (size_t i = 0; i < sizeof(VARIANTS) / sizeof(VARIANTS[0]); ++i) {
        if (strcmp(VARIANTS[i].name, variant) == 0)
            run = VARIANTS[i].run;
    }
    if (run == NULL) {
        fprintf(stderr, "SYSV-SHM-STRESS-ERROR unknown variant: %s\n", variant);
        return 2;
    }
    signal(SIGALRM, on_alarm);
    if (alarm_seconds)
        alarm(alarm_seconds);
    struct counters counters;
    memset(&counters, 0, sizeof(counters));
    struct first_fail first;
    memset(&first, 0, sizeof(first));
    for (unsigned long round = 0; round < rounds; ++round) {
        counters.rounds++;
        run(round, &counters, &first);
    }
    alarm(0);
    if (first.seen) {
        fprintf(stderr,
                "SYSV-SHM-STRESS-FIRST-FAIL variant=%s round=%lu kind=%s "
                "errno=%d data=%lu\n",
                variant, first.round, first.kind, first.error, first.data);
    }
    unsigned long defects = counters.setup_fail + counters.child_fail +
                            counters.value_fail + counters.attach_fail +
                            counters.errno_fail + counters.retire_early_fail +
                            counters.grandchild_fail;
    printf("SYSV-SHM-STRESS variant=%s rounds=%lu completed=%lu setup_fail=%lu "
           "child_fail=%lu value_fail=%lu attach_fail=%lu errno_fail=%lu "
           "retire_early_fail=%lu grandchild_fail=%lu alive_probe=%lu\n",
           variant, counters.rounds, counters.completed, counters.setup_fail,
           counters.child_fail, counters.value_fail, counters.attach_fail,
           counters.errno_fail, counters.retire_early_fail,
           counters.grandchild_fail, counters.alive_probe);
    printf("SYSV-SHM-STRESS-%s %s\n", defects == 0 ? "OK" : "FAIL", variant);
    fflush(stdout);
    return defects == 0 ? 0 : 1;
}
