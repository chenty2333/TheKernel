/* Diagnostic stress driver for the exit-status/concurrent-clone-exit flake.
 *
 * The acceptance test (tests/guest/portable/exit-status.c) discards the
 * discriminating data on failure: it only reports that a worker observed a
 * wrong pid or status. This tool repeats the same clone(SIGCHLD)/exit/wait
 * shape often enough per boot to make the race frequent, and prints exactly
 * what each failing wait returned (requested pid, reaped pid, raw status).
 *
 * Usage: thekernel-exit-status-stress [workers] [rounds]
 *        defaults: 8 workers, 4000 rounds (32000 clone/exit/wait cycles)
 *
 * The parent prints one line per worker plus a total, then OK/FAIL. Every bad
 * observation is printed with its round number; the count is capped so a
 * broken kernel cannot flood the console.
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

#define DEFAULT_WORKERS 8
#define MAX_WORKERS 64
#define DEFAULT_ROUNDS 4000
#define CHILD_EXIT_CODE 37
#define EXPECTED_STATUS (CHILD_EXIT_CODE << 8)
#define MAX_REPORTS 512

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
    int first_failure_round;
    int first_requested;
    int first_reaped;
    int first_status;
    int first_errno;
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

static void report_bad(struct worker_report *report, unsigned round, long requested, long reaped,
                       int status, int error)
{
    if (report->wrong_pid + report->wrong_status + report->wait_errors <= MAX_REPORTS)
        printf("EXIT_STATUS_STRESS_BAD worker=%u round=%u requested=%ld reaped=%ld status=%#x errno=%d\n",
               report->index, round, requested, reaped, (unsigned)status, error);
    if (report->first_failure_round < 0) {
        report->first_failure_round = (int)round;
        report->first_requested = (int)requested;
        report->first_reaped = (int)reaped;
        report->first_status = status;
        report->first_errno = error;
    }
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
    report.first_failure_round = -1;
    report.first_requested = 0;
    report.first_reaped = 0;
    report.first_status = 0;
    report.first_errno = 0;

    for (unsigned round = 0; round < rounds; ++round) {
        unsigned long flags = SIGCHLD | ((round & 1) ? CLONE_FS : 0);
        long child;
        int status = -1;
        pid_t reaped;

        errno = 0;
        child = raw_clone(flags);
        if (child < 0) {
            ++report.wait_errors;
            report_bad(&report, round, -1, -1, status, errno);
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
            report_bad(&report, round, child, reaped, status, errno);
        } else if (reaped != (pid_t)child) {
            ++report.wrong_pid;
            report_bad(&report, round, child, reaped, status, errno);
        } else if (status != EXPECTED_STATUS) {
            ++report.wrong_status;
            report_bad(&report, round, child, reaped, status, errno);
        } else {
            ++report.ok;
        }
    }

    (void)write(write_fd, &report, sizeof(report));
}

int main(int argc, char **argv)
{
    unsigned workers = DEFAULT_WORKERS;
    unsigned rounds = DEFAULT_ROUNDS;
    pid_t pids[MAX_WORKERS];
    int pipes[MAX_WORKERS][2];
    struct worker_report reports[MAX_WORKERS];
    unsigned long total_cycles = 0;
    unsigned long total_wrong_pid = 0;
    unsigned long total_wrong_status = 0;
    unsigned long total_wait_errors = 0;
    int failed = 0;

    if (argc > 1)
        workers = (unsigned)strtoul(argv[1], NULL, 0);
    if (argc > 2)
        rounds = (unsigned)strtoul(argv[2], NULL, 0);
    if (workers == 0 || workers > MAX_WORKERS)
        workers = DEFAULT_WORKERS;
    if (rounds == 0)
        rounds = DEFAULT_ROUNDS;

    printf("EXIT_STATUS_STRESS_START workers=%u rounds=%u\n", workers, rounds);
    fflush(stdout);

    for (unsigned index = 0; index < workers; ++index) {
        pid_t worker;

        if (pipe(pipes[index])) {
            printf("EXIT_STATUS_STRESS_FAIL pipe index=%u errno=%d\n", index, errno);
            return 1;
        }
        worker = fork();
        if (worker < 0) {
            printf("EXIT_STATUS_STRESS_FAIL fork index=%u errno=%d\n", index, errno);
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

    /* Reap every worker before reading any pipe: `waitpid(worker)` must never
     * be in flight while a worker is still creating children, or the parent
     * would collect the worker's children and steal its waits. */
    for (unsigned index = 0; index < workers; ++index) {
        int status = -1;
        pid_t reaped;

        do {
            reaped = waitpid(pids[index], &status, 0);
        } while (reaped < 0 && errno == EINTR);
        if (reaped != pids[index] || status != 0) {
            printf("EXIT_STATUS_STRESS_FAIL worker_exit index=%u reaped=%d want=%d status=%#x\n",
                   index, (int)reaped, (int)pids[index], (unsigned)status);
            failed = 1;
        }
    }

    for (unsigned index = 0; index < workers; ++index) {
        size_t got = 0;
        char *into = (char *)&reports[index];

        reports[index].first_failure_round = -1;
        while (got < sizeof(reports[index])) {
            ssize_t count = read(pipes[index][0], into + got, sizeof(reports[index]) - got);
            if (count <= 0)
                break;
            got += (size_t)count;
        }
        close(pipes[index][0]);
        if (got != sizeof(reports[index])) {
            printf("EXIT_STATUS_STRESS_FAIL worker_report index=%u got=%zu\n", index, got);
            failed = 1;
        }
    }

    for (unsigned index = 0; index < workers; ++index) {
        const struct worker_report *report = &reports[index];

        total_cycles += report->cycles;
        total_wrong_pid += report->wrong_pid;
        total_wrong_status += report->wrong_status;
        total_wait_errors += report->wait_errors;
        printf("EXIT_STATUS_STRESS_WORKER worker=%u rounds=%u cycles=%lu ok=%lu wrong_pid=%lu "
               "wrong_status=%lu wait_errors=%lu pid_first=%ld pid_last=%ld first_round=%d "
               "first_req=%d first_reaped=%d first_status=%#x first_errno=%d\n",
               index, report->rounds_done, report->cycles, report->ok, report->wrong_pid,
               report->wrong_status, report->wait_errors, report->first_child_pid,
               report->last_child_pid, report->first_failure_round, report->first_requested,
               report->first_reaped, (unsigned)report->first_status, report->first_errno);
    }

    printf("EXIT_STATUS_STRESS_DONE workers=%u rounds=%u cycles=%lu ok=%lu wrong_pid=%lu "
           "wrong_status=%lu wait_errors=%lu\n",
           workers, rounds, total_cycles, total_cycles - total_wrong_pid - total_wrong_status -
               total_wait_errors,
           total_wrong_pid, total_wrong_status, total_wait_errors);
    fflush(stdout);
    if (failed || total_wrong_pid || total_wrong_status || total_wait_errors) {
        puts("THEKERNEL_EXIT_STATUS_STRESS_FAIL");
        return 1;
    }
    puts("THEKERNEL_EXIT_STATUS_STRESS_OK");
    return 0;
}
