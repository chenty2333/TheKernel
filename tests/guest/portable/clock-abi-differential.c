/*
 * Portable Linux/x86_64 clock ABI differential test.
 *
 * The program is built once and executed unchanged on real Linux and on
 * TheKernel; a differential harness compares the marker lines emitted on
 * stdout.  No clock reading, PID or duration is ever printed: the assertions
 * are relations between two readings taken by this process, so they hold on
 * any machine and at any wall-clock time.
 *
 * The property under test is the one POSIX and Linux give CLOCK_MONOTONIC
 * (`kernel/time/posix-timers.c`: `posix_clocks[CLOCK_MONOTONIC]` is
 * `posix_clock_monotonic` over `tk_core.timekeeper`, and
 * `include/linux/timekeeping.h` declares it "Monotonic time since some
 * unspecified starting point"): it starts near zero on a freshly booted
 * machine, counts real elapsed time at a stable rate, never jumps backwards,
 * and is the clock CLOCK_BOOTTIME (the same counter plus suspend time,
 * `timekeeping.c: ktime_get_boottime_ts64()`) is never behind.  The first
 * field of /proc/uptime is CLOCK_BOOTTIME as well
 * (`fs/proc/uptime.c: ktime_get_boottime_ts64(&uptime)`).
 *
 * Every comparison is deliberately loose: a guest may stop the vCPU, may be
 * preempted for an arbitrary time, and may run on a host with a coarse clock
 * source.  What is asserted is that time moves forward by roughly what was
 * slept or spun, never that it moves by an exact amount.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* A busy-wait this long must be observed by CLOCK_MONOTONIC and
 * CLOCK_MONOTONIC_RAW as at least a third of it, and by no more than the
 * generous upper bound that catches a clock running at the wrong rate. */
#define SPIN_NS 100000000LL
#define SPIN_LOWER_NS (SPIN_NS / 3)
#define SPIN_UPPER_NS (SPIN_NS * 50)

/* The sleep is the sharper instrument: an interval timer must resume the
 * process, so the clock has to have advanced by about the sleep. */
#define SLEEP_NS 300000000LL
#define SLEEP_LOWER_NS (SLEEP_NS - 100000000LL)
#define SLEEP_UPPER_NS (SLEEP_NS + 2000000000LL)

/* Two clocks read microseconds apart stay within this of each other when
 * neither has a rate adjustment applied. */
#define SKEW_BOUND_NS 100000000LL

/* How many readings a spin may take without the clock moving at all before
 * the case calls the clock stopped.  A working CLOCK_MONOTONIC reaches the
 * bound in far fewer readings; a frozen one would otherwise spin forever,
 * because the bound it waits for is the very reading that never arrives. */
#define SPIN_STALL_ITERATIONS 2000000UL

/* A fresh guest has been up for well under a day; a wall clock in 2020-2100
 * brackets every machine this test runs on.  Both are properties of the
 * machine the case runs on rather than of the ABI, so the epoch bound can be
 * overridden to smoke-test the rest of the case on a long-running host. */
#ifndef UPTIME_UPPER_NS
#define UPTIME_UPPER_NS 86400000000000LL
#endif
#define REALTIME_LOWER_SEC 1600000000LL
#define REALTIME_UPPER_SEC 4102444800LL

static int fail(const char *stage, int error_number) {
    fprintf(stderr, "THEKERNEL_CLOCK_ABI_FAIL %s errno=%d\n", stage,
            error_number);
    return 1;
}

static void marker(const char *line) {
    puts(line);
    fflush(stdout);
}

/*
 * Structured ABI record for tools/qemu_runner/abi_differential.py.
 *
 * The runner requires every case to be bracketed by
 * THEKERNEL_ABI_CASE / THEKERNEL_ABI_ASSERT* / THEKERNEL_ABI_RESULT with the
 * "<case>.portable-differential" name and to list the case's assertion tokens
 * with the outcome "pass".  The same function also prints the plain marker
 * line so the program stays readable on its own.
 */
static void record(const char *case_name, const char *assertions,
                   const char *marker_line) {
    char buffer[256];
    size_t length = strlen(assertions);

    if (length >= sizeof(buffer)) {
        fprintf(stderr, "THEKERNEL_CLOCK_ABI_FAIL record %s\n", case_name);
        exit(1);
    }
    memcpy(buffer, assertions, length + 1);
    printf("THEKERNEL_ABI_CASE %s.portable-differential\n", case_name);
    for (char *token = strtok(buffer, " "); token != NULL;
         token = strtok(NULL, " ")) {
        printf("THEKERNEL_ABI_ASSERT %s.portable-differential %s pass\n",
               case_name, token);
    }
    marker(marker_line);
    printf("THEKERNEL_ABI_RESULT %s.portable-differential pass\n", case_name);
    fflush(stdout);
}

/* One sample of every clock the case reads.  `ok` records which of the four
 * clock_gettime() calls succeeded: a clock the kernel does not implement must
 * fail with EINVAL rather than with anything else. */
struct sample {
    struct timespec monotonic;
    struct timespec raw;
    struct timespec boottime;
    struct timespec realtime;
    int monotonic_errno;
    int raw_errno;
    int boottime_errno;
    int realtime_errno;
};

static int sample_clocks(struct sample *out) {
    memset(out, 0, sizeof(*out));

    errno = 0;
    if (clock_gettime(CLOCK_MONOTONIC, &out->monotonic) != 0) {
        out->monotonic_errno = errno != 0 ? errno : EPROTO;
    }
    errno = 0;
    if (clock_gettime(CLOCK_MONOTONIC_RAW, &out->raw) != 0) {
        out->raw_errno = errno != 0 ? errno : EPROTO;
    }
    errno = 0;
    if (clock_gettime(CLOCK_BOOTTIME, &out->boottime) != 0) {
        out->boottime_errno = errno != 0 ? errno : EPROTO;
    }
    errno = 0;
    if (clock_gettime(CLOCK_REALTIME, &out->realtime) != 0) {
        out->realtime_errno = errno != 0 ? errno : EPROTO;
    }
    return out->monotonic_errno == 0 && out->raw_errno == 0 &&
                   out->boottime_errno == 0 && out->realtime_errno == 0
               ? 0
               : -1;
}

static int64_t to_ns(const struct timespec *value) {
    return (int64_t)value->tv_sec * 1000000000LL + (int64_t)value->tv_nsec;
}

/* A timespec is normalized and non-negative.  The nanoseconds field of every
 * clock_gettime() result is in [0, 1000000000); a value outside it, or a
 * negative seconds field, is a kernel that failed to carry. */
static int normalized(const struct timespec *value) {
    return value->tv_nsec >= 0 && value->tv_nsec < 1000000000L &&
           value->tv_sec >= 0;
}

static int64_t uptime_ns(void) {
    char buffer[128];
    int fd = open("/proc/uptime", O_RDONLY);
    ssize_t length;
    char *end = NULL;
    double seconds;

    if (fd < 0) {
        return -1;
    }
    length = read(fd, buffer, sizeof(buffer) - 1);
    close(fd);
    if (length <= 0) {
        return -1;
    }
    buffer[length] = '\0';
    errno = 0;
    seconds = strtod(buffer, &end);
    if (end == buffer || !(seconds >= 0.0)) {
        return -1;
    }
    return (int64_t)(seconds * 1000000000.0);
}

/* Burn CPU until `ns` of CLOCK_MONOTONIC time have passed, and return the
 * number of iterations that took.  Zero means the clock could not be read or
 * did not move at all: a clock that never reaches the bound is reported to the
 * caller instead of spinning the case into the harness timeout. */
static unsigned long spin_ns(int64_t ns) {
    struct timespec start;
    struct timespec bound;
    unsigned long iterations = 0;

    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return 0;
    }
    bound = start;
    bound.tv_sec += (time_t)(ns / 1000000000LL);
    bound.tv_nsec += (long)(ns % 1000000000LL);
    if (bound.tv_nsec >= 1000000000L) {
        bound.tv_sec += 1;
        bound.tv_nsec -= 1000000000L;
    }
    for (;;) {
        struct timespec now;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            return 0;
        }
        iterations++;
        if (now.tv_sec > bound.tv_sec ||
            (now.tv_sec == bound.tv_sec && now.tv_nsec >= bound.tv_nsec)) {
            return iterations;
        }
        if (iterations == SPIN_STALL_ITERATIONS &&
            now.tv_sec == start.tv_sec && now.tv_nsec == start.tv_nsec) {
            return 0;
        }
    }
}

static int test_monotonic_progress(void) {
    struct sample before;
    struct sample after;
    struct timespec requested;
    struct timespec remaining;
    int64_t spin_delta;
    int64_t sleep_delta;
    int64_t raw_delta;
    int64_t skew;
    int64_t uptime;

    if (sample_clocks(&before) != 0 || sample_clocks(&after) != 0) {
        return fail("clock_gettime", before.monotonic_errno != 0
                                         ? before.monotonic_errno
                                     : before.raw_errno != 0 ? before.raw_errno
                                     : before.boottime_errno != 0
                                         ? before.boottime_errno
                                         : before.realtime_errno);
    }
    if (!normalized(&before.monotonic) || !normalized(&after.monotonic) ||
        !normalized(&before.raw) || !normalized(&after.raw) ||
        !normalized(&before.boottime) || !normalized(&after.boottime) ||
        !normalized(&before.realtime) || !normalized(&after.realtime)) {
        return fail("clock-timespec-normalized", EPROTO);
    }

    /* CLOCK_MONOTONIC starts at an unspecified point, but a machine that has
     * just booted is not days into its monotonic epoch, and no clock may run
     * backwards between two readings. */
    if (before.monotonic.tv_sec < 0 ||
        before.monotonic.tv_sec > UPTIME_UPPER_NS / 1000000000LL) {
        return fail("monotonic-epoch", EPROTO);
    }

    /* CLOCK_BOOTTIME is CLOCK_MONOTONIC plus the time spent suspended:
     * "ktime_get_boottime_ts64()" cannot be behind it. */
    if (to_ns(&before.boottime) < to_ns(&before.monotonic) ||
        to_ns(&after.boottime) < to_ns(&after.monotonic)) {
        return fail("boottime-below-monotonic", EPROTO);
    }

    /* CLOCK_REALTIME is the wall clock; CLOCK_MONOTONIC is not. */
    if (before.realtime.tv_sec < REALTIME_LOWER_SEC ||
        before.realtime.tv_sec > REALTIME_UPPER_SEC) {
        return fail("realtime-bracket", EPROTO);
    }
    if (before.monotonic.tv_sec >= REALTIME_LOWER_SEC) {
        return fail("monotonic-not-a-wall-clock", EPROTO);
    }

    /* /proc/uptime's first field is the same counter as CLOCK_BOOTTIME, read
     * a moment later, so it is ahead by the time this process took to read
     * it and never behind the boottime sample taken before it. */
    uptime = uptime_ns();
    if (uptime < 0) {
        return fail("proc-uptime", errno);
    }
    if (uptime + SKEW_BOUND_NS < to_ns(&before.boottime) ||
        uptime > to_ns(&before.boottime) + SLEEP_UPPER_NS) {
        return fail("uptime-matches-boottime", EPROTO);
    }

    /* Time advances while this process burns CPU. */
    if (spin_ns(SPIN_NS) == 0) {
        return fail("monotonic-spin-clock", errno);
    }
    if (sample_clocks(&after) != 0) {
        return fail("clock_gettime-after-spin", EPROTO);
    }
    spin_delta = to_ns(&after.monotonic) - to_ns(&before.monotonic);
    raw_delta = to_ns(&after.raw) - to_ns(&before.raw);
    if (spin_delta < SPIN_LOWER_NS || spin_delta > SPIN_UPPER_NS) {
        return fail("monotonic-advances", EPROTO);
    }
    /* CLOCK_MONOTONIC_RAW is the same counter without the NTP adjustment, so
     * over one interval it moves by the same amount.  Only a clock that is
     * stopped, started late, or running at a different rate drifts by more
     * than the skew bound here. */
    skew = raw_delta - spin_delta;
    if (skew < 0) {
        skew = -skew;
    }
    if (raw_delta < SPIN_LOWER_NS || skew > SKEW_BOUND_NS) {
        return fail("monotonic-raw-tracks-monotonic", EPROTO);
    }

    /* Sleeping is the sharper test: the process is not running, so only the
     * kernel's clock can account for the interval, and POSIX requires
     * CLOCK_MONOTONIC to include it. */
    before = after;
    requested.tv_sec = (time_t)(SLEEP_NS / 1000000000LL);
    requested.tv_nsec = (long)(SLEEP_NS % 1000000000LL);
    remaining.tv_sec = 0;
    remaining.tv_nsec = 0;
    errno = 0;
    if (clock_nanosleep(CLOCK_MONOTONIC, 0, &requested, &remaining) != 0) {
        return fail("clock_nanosleep", errno);
    }
    if (sample_clocks(&after) != 0) {
        return fail("clock_gettime-after-sleep", EPROTO);
    }
    sleep_delta = to_ns(&after.monotonic) - to_ns(&before.monotonic);
    if (sleep_delta < SLEEP_LOWER_NS || sleep_delta > SLEEP_UPPER_NS) {
        return fail("monotonic-sleep-delta", EPROTO);
    }
    if (to_ns(&after.boottime) - to_ns(&before.boottime) < SLEEP_LOWER_NS) {
        return fail("boottime-sleep-delta", EPROTO);
    }
    /* The wall clock advanced over the same interval: a frozen or rewinding
     * CLOCK_REALTIME is a different clock than the monotonic one. */
    if (to_ns(&after.realtime) <= to_ns(&before.realtime)) {
        return fail("realtime-advances", EPROTO);
    }
    return 0;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    if (test_monotonic_progress() != 0) {
        return 1;
    }
    record("clock-abi",
           "MONOTONIC_EPOCH TIMESPEC_NORMALIZED BOOTTIME_NOT_BELOW_MONOTONIC "
           "REALTIME_WALL_CLOCK UPTIME_IS_BOOTTIME MONOTONIC_ADVANCES "
           "RAW_TRACKS_MONOTONIC SLEEP_ADVANCES_MONOTONIC",
           "THEKERNEL_CLOCK_ABI_OK monotonic_advances=1 raw_tracks=1 "
           "sleep_advances=1 uptime_is_boottime=1");
    marker("THEKERNEL_CLOCK_ABI_DIFFERENTIAL_OK");
    return 0;
}
