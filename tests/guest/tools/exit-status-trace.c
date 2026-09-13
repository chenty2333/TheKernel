/* Failing-invocation probe for the exit-status/concurrent-clone-exit race.
 *
 * This tool is a diagnostic, not an acceptance test: it repeats exactly the
 * `concurrent_clone_exit()` worker of tests/guest/portable/exit-status.c (four
 * workers, each cloning SIGCHLD / SIGCHLD|CLONE_FS children that exit with 37
 * and reaping them with a pid-targeted `waitpid`), many times in one guest
 * boot, and records the precise failing observation:
 *
 *   * the requested pid, the pid `waitpid` returned and the raw status,
 *   * the round number inside the worker,
 *   * whether the returned status was a signal instead of an exit code.
 *
 * On the first failure of a worker it also prints the kernel's own trace of
 * pid-targeted waits and process-exit publications
 * (`/proc/sys/kernel/exit-status`), which pairs a failed wait with the child
 * that was cloned for it and with the wait status its zombie published.
 *
 * Usage: thekernel-exit-status-trace [batches] [workers] [rounds]
 *        defaults: 20 batches, 4 workers, 32 rounds
 *
 * Exit status: 0 when every batch was clean, 1 otherwise.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define SYS_CLONE 56
#define SYS_EXIT 60
#define SYS_EXIT_GROUP 231

#ifndef SIGCHLD
#define SIGCHLD 17
#endif
#ifndef CLONE_FS
#define CLONE_FS 0x00000200
#endif

#define MAX_WORKERS 32
#define CHILD_EXIT_CODE 37
#define EXPECTED_STATUS (CHILD_EXIT_CODE << 8)

enum { FAIL_NONE = 0, FAIL_CLONE = 1, FAIL_ECHILD = 2, FAIL_WRONG_PID = 3, FAIL_WRONG_STATUS = 4 };

struct worker_report {
    unsigned index;
    unsigned rounds_done;
    unsigned long cycles;
    unsigned long ok;
    unsigned long wrong_pid;
    unsigned long wrong_status;
    unsigned long wait_errors;
    long first_child_pid;
    long last_child_pid;
    int first_round;
    int first_requested;
    int first_reaped;
    int first_status;
    int first_errno;
    unsigned first_kind;
};

static long raw_clone(unsigned long flags)
{
    long ret;
    register long r10 __asm__("r10") = 0;
    register long r8 __asm__("r8") = 0;
    register long r9 __asm__("r9") = 0;

    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"((long)SYS_CLONE), "D"((long)flags), "S"(0L), "d"(0L), "r"(r10), "r"(r8),
                       "r"(r9)
                     : "rcx", "r11", "memory");
    return ret;
}

static void raw_exit(int code)
{
    __asm__ volatile("syscall" : : "a"((long)SYS_EXIT), "D"((long)code) : "rcx", "r11", "memory");
    __builtin_unreachable();
}

static void raw_exit_group(int code)
{
    __asm__ volatile("syscall"
                     :
                     : "a"((long)SYS_EXIT_GROUP), "D"((long)code)
                     : "rcx", "r11", "memory");
    __builtin_unreachable();
}

static void note_failure(struct worker_report *report, unsigned round, unsigned kind, long requested,
                         long reaped, int status, int error)
{
    if (report->first_round < 0) {
        report->first_round = (int)round;
        report->first_requested = (int)requested;
        report->first_reaped = (int)reaped;
        report->first_status = status;
        report->first_errno = error;
        report->first_kind = kind;
    }
}

static int status_is_signal(int status)
{
    return (status & 0x7f) != 0 && (status & 0xff) != 0x7f;
}

static void worker_main(unsigned index, unsigned rounds, int write_fd)
{
    struct worker_report report;

    report.index = index;
    report.rounds_done = 0;
    report.cycles = 0;
    report.ok = 0;
    report.wrong_pid = 0;
    report.wrong_status = 0;
    report.wait_errors = 0;
    report.first_child_pid = -1;
    report.last_child_pid = -1;
    report.first_round = -1;
    report.first_requested = 0;
    report.first_reaped = 0;
    report.first_status = 0;
    report.first_errno = 0;
    report.first_kind = FAIL_NONE;

    for (unsigned round = 0; round < rounds; ++round) {
        unsigned long flags = SIGCHLD | ((round & 1) ? CLONE_FS : 0);
        long child;
        int status = -1;
        pid_t reaped;

        errno = 0;
        child = raw_clone(flags);
        if (child < 0) {
            ++report.wait_errors;
            note_failure(&report, round, FAIL_CLONE, -1, -1, status, errno);
            break;
        }
        if (child == 0) {
            if (round & 1)
                raw_exit(CHILD_EXIT_CODE);
            raw_exit_group(CHILD_EXIT_CODE);
        }
        report.rounds_done = round + 1;
        if (report.first_child_pid < 0)
            report.first_child_pid = child;
        report.last_child_pid = child;

        do {
            reaped = waitpid((pid_t)child, &status, 0);
        } while (reaped < 0 && errno == EINTR);

        ++report.cycles;
        if (reaped < 0) {
            ++report.wait_errors;
            note_failure(&report, round, FAIL_ECHILD, child, reaped, status, errno);
            break;
        } else if (reaped != (pid_t)child) {
            ++report.wrong_pid;
            note_failure(&report, round, FAIL_WRONG_PID, child, reaped, status, errno);
            break;
        } else if (status != EXPECTED_STATUS) {
            ++report.wrong_status;
            note_failure(&report, round, FAIL_WRONG_STATUS, child, reaped, status, errno);
            break;
        } else {
            ++report.ok;
        }
    }

    (void)write(write_fd, &report, sizeof(report));
}

static void dump_kernel_trace(void)
{
    FILE *trace = fopen("/proc/sys/kernel/exit-status", "r");

    puts("--- KERNEL TRACE BEGIN ---");
    if (trace == NULL) {
        printf("EXIT_STATUS_TRACE_UNAVAILABLE errno=%d\n", errno);
    } else {
        char line[512];
        size_t lines = 0;

        while (fgets(line, sizeof(line), trace) != NULL) {
            fputs(line, stdout);
            if (++lines > 1200) {
                puts("EXIT_STATUS_TRACE_TRUNCATED");
                break;
            }
        }
        fclose(trace);
    }
    puts("--- KERNEL TRACE END ---");
}

int main(int argc, char **argv)
{
    unsigned batches = 20;
    unsigned workers = 4;
    unsigned rounds = 32;
    pid_t pids[MAX_WORKERS];
    int pipes[MAX_WORKERS][2];
    struct worker_report reports[MAX_WORKERS];
    unsigned long total_cycles = 0;
    unsigned long total_ok = 0;
    unsigned long total_bad = 0;
    int failed = 0;

    if (argc > 1)
        batches = (unsigned)strtoul(argv[1], NULL, 0);
    if (argc > 2)
        workers = (unsigned)strtoul(argv[2], NULL, 0);
    if (argc > 3)
        rounds = (unsigned)strtoul(argv[3], NULL, 0);
    if (batches == 0)
        batches = 1;
    if (workers == 0 || workers > MAX_WORKERS)
        workers = 4;
    if (rounds == 0)
        rounds = 32;

    printf("EXIT_STATUS_TRACE_START batches=%u workers=%u rounds=%u\n", batches, workers, rounds);
    fflush(stdout);

    for (unsigned batch = 0; batch < batches; ++batch) {
        int batch_failed = 0;

        for (unsigned index = 0; index < workers; ++index) {
            pid_t worker;

            if (pipe(pipes[index])) {
                printf("EXIT_STATUS_TRACE_FAIL pipe batch=%u index=%u errno=%d\n", batch, index,
                       errno);
                return 1;
            }
            worker = fork();
            if (worker < 0) {
                printf("EXIT_STATUS_TRACE_FAIL fork batch=%u index=%u errno=%d\n", batch, index,
                       errno);
                return 1;
            }
            if (worker == 0) {
                close(pipes[index][0]);
                worker_main(index, rounds, pipes[index][1]);
                _exit(0);
            }
            close(pipes[index][1]);
            pids[index] = worker;
        }

        for (unsigned index = 0; index < workers; ++index) {
            int status = -1;
            pid_t reaped;

            do {
                reaped = waitpid(pids[index], &status, 0);
            } while (reaped < 0 && errno == EINTR);
            if (reaped != pids[index] || status != 0) {
                printf("EXIT_STATUS_TRACE_FAIL worker_exit batch=%u index=%u reaped=%d want=%d "
                       "status=%#x signal=%d\n",
                       batch, index, (int)reaped, (int)pids[index], (unsigned)status,
                       status_is_signal(status));
                failed = 1;
                batch_failed = 1;
            }
        }

        for (unsigned index = 0; index < workers; ++index) {
            size_t got = 0;
            const char *from = (const char *)&reports[index];

            reports[index].first_round = -1;
            while (got < sizeof(reports[index])) {
                ssize_t count =
                    read(pipes[index][0], (char *)from + got, sizeof(reports[index]) - got);
                if (count <= 0)
                    break;
                got += (size_t)count;
            }
            close(pipes[index][0]);
            if (got != sizeof(reports[index])) {
                printf("EXIT_STATUS_TRACE_FAIL worker_report batch=%u index=%u got=%zu\n", batch,
                       index, got);
                failed = 1;
                batch_failed = 1;
            }
        }

        for (unsigned index = 0; index < workers; ++index) {
            const struct worker_report *report = &reports[index];

            total_cycles += report->cycles;
            total_ok += report->ok;
            total_bad += report->wrong_pid + report->wrong_status + report->wait_errors;
            if (report->first_kind != FAIL_NONE) {
                printf("EXIT_STATUS_TRACE_BAD batch=%u worker=%u kind=%u round=%d req=%d reaped=%d "
                       "status=%#x errno=%d rounds_done=%u first_child_pid=%ld "
                       "last_child_pid=%ld\n",
                       batch, index, report->first_kind, report->first_round,
                       report->first_requested, report->first_reaped,
                       (unsigned)report->first_status, report->first_errno, report->rounds_done,
                       report->first_child_pid, report->last_child_pid);
                fflush(stdout);
                failed = 1;
                batch_failed = 1;
            }
        }

        if (batch_failed) {
            /* One dump is enough: the ring holds the 512 events around the
             * failure, and printing it costs a guest round trip. */
            dump_kernel_trace();
            fflush(stdout);
            printf("EXIT_STATUS_TRACE_DONE batches=%u workers=%u rounds=%u cycles=%lu ok=%lu bad=%lu "
                   "failed=1\n",
                   batches, workers, rounds, total_cycles, total_ok, total_bad);
            fflush(stdout);
            return 1;
        }
        printf("EXIT_STATUS_TRACE_BATCH batch=%u ok=1\n", batch);
        fflush(stdout);
    }

    printf("EXIT_STATUS_TRACE_DONE batches=%u workers=%u rounds=%u cycles=%lu ok=%lu bad=%lu "
           "failed=%d\n",
           batches, workers, rounds, total_cycles, total_ok, total_bad, failed);
    fflush(stdout);
    return failed ? 1 : 0;
}
