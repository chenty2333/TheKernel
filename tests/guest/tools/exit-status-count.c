/* Repeated-invocation acceptance profile for the exit-status race.
 *
 * The acceptance case is a program, not a loop: `tests/guest/system-init.c`
 * runs `/opt/thekernel-tests/portable/exit-status` once per suite boot, and the
 * race only shows up when that program is invoked many times. This tool makes
 * the invocation profile measurable inside a single boot:
 *
 *   * each iteration fork+execs the acceptance binary, so every iteration is a
 *     fresh process image exactly like a suite boot;
 *   * the child's output is inherited, so a failure prints the binary's own
 *     `THEKERNEL_EXIT_STATUS_FAIL` line on the console;
 *   * on the first failure the parent prints the kernel's pid-targeted wait
 *     trace (`/proc/sys/kernel/exit-status`) while the ring still holds the
 *     events around it;
 *   * the tool prints one bounded summary line, so a measurement run does not
 *     pour thousands of lines into the console.
 *
 * Usage: thekernel-exit-status-count [iterations] [dump]
 *        defaults: 200 iterations, dump=1
 *
 * `dump` selects whether a failing iteration prints the kernel trace. The
 * trace is large; on a serial console it can overrun the runner, so a
 * measurement run that only wants a count passes `0`.
 *
 * Exit status: number of failed iterations (capped at 125).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>

#define BINARY "/opt/thekernel-tests/portable/exit-status"
#define MAX_REPORTED 8

static void dump_kernel_trace(void)
{
    FILE *trace = fopen("/proc/sys/kernel/exit-status", "r");

    puts("--- KERNEL TRACE BEGIN ---");
    if (trace == NULL) {
        printf("EXIT_STATUS_TRACE_UNAVAILABLE errno=%d\n", errno);
    } else {
        char line[512];
        unsigned long lines = 0;

        while (fgets(line, sizeof(line), trace) != NULL) {
            fputs(line, stdout);
            if (++lines > 1400) {
                puts("EXIT_STATUS_TRACE_TRUNCATED");
                break;
            }
        }
        fclose(trace);
    }
    puts("--- KERNEL TRACE END ---");
    fflush(stdout);
}

int main(int argc, char **argv)
{
    unsigned iterations = 200;
    unsigned dump = 1;
    unsigned long fails = 0;
    unsigned long reported = 0;

    if (argc > 1) {
        iterations = (unsigned)strtoul(argv[1], NULL, 0);
        if (iterations == 0)
            iterations = 1;
    }
    if (argc > 2)
        dump = (unsigned)strtoul(argv[2], NULL, 0) != 0;

    printf("EXIT_STATUS_COUNT_START iterations=%u dump=%u binary=%s\n", iterations, dump, BINARY);
    fflush(stdout);

    for (unsigned i = 1; i <= iterations; ++i) {
        pid_t child = fork();
        int status = -1;
        pid_t reaped;

        if (child < 0) {
            printf("EXIT_STATUS_COUNT_FAIL fork iteration=%u errno=%d\n", i, errno);
            return 125;
        }
        if (child == 0) {
            execl(BINARY, BINARY, (char *)NULL);
            _exit(127);
        }
        do {
            reaped = waitpid(child, &status, 0);
        } while (reaped < 0 && errno == EINTR);

        if (reaped != child || status != 0) {
            ++fails;
            if (reported < MAX_REPORTED) {
                ++reported;
                printf("EXIT_STATUS_COUNT_FAIL iteration=%u reaped=%d want=%d status=%#x "
                       "signal=%d\n",
                       i, (int)reaped, (int)child, (unsigned)status,
                       (status & 0x7f) != 0 && (status & 0xff) != 0x7f);
                fflush(stdout);
                if (dump)
                    dump_kernel_trace();
            }
        }
    }

    printf("EXIT_STATUS_COUNT_DONE iterations=%u fails=%lu\n", iterations, fails);
    fflush(stdout);
    return fails > 125 ? 125 : (int)fails;
}
