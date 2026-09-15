/*
 * Portable differential contract for the time syscalls whose Linux behaviour
 * TheKernel previously approximated:
 *
 *   gettimeofday(2)   96    settimeofday(2) 164   clock_settime(2) 227
 *   adjtimex(2)      159    clock_adjtime(2) 305  timer_create(2) 222
 *   timer_getoverrun(2) 225 clock_getres(2) 229   timer_delete(2) 223
 *   clock_nanosleep(2) 230
 *
 * Every record this program prints is compared against the same program
 * running on a Linux 7.2.3 guest, so each assertion must hold on both.  The
 * expectations below are transcribed from that release:
 *
 *   kernel/time/time.c:140-155         gettimeofday(tz) reports sys_tz
 *   kernel/time/time.c:169-197         do_sys_settimeofday64: bound, capability,
 *     timezone range, sys_tz store, then the one-shot timekeeping_warp_clock()
 *   kernel/time/time.c:199-222         settimeofday validation order
 *   kernel/time/timekeeping.c:1781     timekeeping_warp_clock (tz_minuteswest*60)
 *   kernel/time/posix-timers.c:1123    clock_settime: clock classification,
 *     copy-in, then the clock's own setter
 *   kernel/time/posix-cpu-timers.c:180 posix_cpu_clock_set is EPERM on a valid
 *     target and EINVAL when pid_for_clock() finds no task
 *   kernel/time/timekeeping.c:1658     do_settimeofday64 bounds and floor
 *   include/linux/time64.h:118-127     timespec64_valid_settod()
 *   include/linux/timex.h:151          NTP_INTERVAL_FREQ is (HZ)
 *   kernel/time/ntp.c:331,806          the ADJ_OFFSET scale and its render
 *   kernel/time/timekeeping.c:2830     timekeeping_validate_timex()
 *   kernel/time/ntp.c:90-100,696-846   NTP state, modes and rendering
 *   kernel/time/posix-timers.c:404-548 timer_create notification and ordering
 *   kernel/time/posix-timers.c:279-289 timer_getoverrun is it_overrun_last
 *   kernel/time/posix-cpu-timers.c:158-176 CPU clock resolutions and the
 *     pid_for_clock() lookup that must succeed first
 *   kernel/time/posix-timers.c:1383    clock_nanosleep clock table and ordering
 *   kernel/time/posix-cpu-timers.c:1630 refusing a self thread-clock sleep
 *   kernel/time/posix-timers.c:1355-1363 clock_nanosleep reads TIMER_ABSTIME
 *     and ignores every other flag bit
 *   kernel/time/alarmtimer.c:775-782   the alarm path's RTC/flags/capability
 *     order
 *   kernel/time/posix-timers.c:222-227 CLOCK_MONOTONIC_RAW adds the monotonic
 *     time-namespace offset, via include/linux/time_namespace.h:73-78
 *   kernel/time/posix-timers.c:1160-1171 do_clock_adjtime: a clock without a
 *     clock_adj operation is EOPNOTSUPP and an unknown id is EINVAL
 *   include/uapi/linux/timex.h         the ADJ_ and STA_ values used here
 *
 * Assertions that depend on the oracle kernel's internal HZ or on whether its
 * RTC driver is loaded are deliberately avoided.  The one HZ-dependent value
 * is asserted as the *pair* that has to hold at any rate: `offset + ADJ_NANO`
 * renders exactly when the phase is a multiple of the rate and one nanosecond
 * low otherwise (kernel/time/ntp.c:331,806, so 25 renders as 24 at HZ=1000),
 * which is what both guests must agree on.  CPU clock resolutions are only
 * compared against one nanosecond, and the wake-alarm clock is asserted
 * through its copy-out ordering and through whatever errno the RTC
 * availability implies.  The last case drops to an unprivileged uid to observe
 * the settimeofday(2) validation order and runs after every privileged case.
 *
 * The time-namespace assertion unshares CLONE_NEWTIME, which both guests
 * support (the oracle's .config has CONFIG_TIME_NS=y); if the unshare or the
 * offset write fails, the case fails rather than passing vacuously.
 */
#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/timex.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define KIND "raw-differential"

/* CPU clock ids are encoded exactly as include/uapi/linux/posix-timers.h and
 * Makefile's MAKE_PROCESS_CPUCLOCK/MAKE_THREAD_CPUCLOCK do; the shift is
 * performed in unsigned arithmetic so it stays well defined. */
#define ABI_CPUCLOCK_PROF 0
#define ABI_CPUCLOCK_VIRT 1
#define ABI_CPUCLOCK_SCHED 2
#define ABI_CPUCLOCK_PERTHREAD 4
#define ABI_CPUCLOCK_PROCESS(pid, which) ((int)(~(unsigned)(pid) << 3 | (unsigned)(which)))
#define ABI_CPUCLOCK_THREAD(tid, which) \
    ((int)(~(unsigned)(tid) << 3 | (unsigned)((which) | ABI_CPUCLOCK_PERTHREAD)))
#define ABI_CLOCKFD_ID(fd) ((int)((~(unsigned)(fd) << 3) | 3u))

/* include/uapi/linux/sched.h: CLONE_NEWTIME.  glibc before 2.36 does not
 * define it, so it is spelled out here. */
#ifndef CLONE_NEWTIME
#define CLONE_NEWTIME 0x00000080
#endif

/* UAPI timex bits, spelled out so this file cannot drift with a C library. */
#define ABI_ADJ_OFFSET 0x0001
#define ABI_ADJ_FREQUENCY 0x0002
#define ABI_ADJ_MAXERROR 0x0004
#define ABI_ADJ_ESTERROR 0x0008
#define ABI_ADJ_STATUS 0x0010
#define ABI_ADJ_TIMECONST 0x0020
#define ABI_ADJ_TAI 0x0080
#define ABI_ADJ_SETOFFSET 0x0100
#define ABI_ADJ_MICRO 0x1000
#define ABI_ADJ_NANO 0x2000
#define ABI_ADJ_TICK 0x4000
#define ABI_ADJ_OFFSET_SINGLESHOT 0x0001 /* kernel-side spelling */
#define ABI_ADJ_OFFSET_READONLY 0x2000   /* ADJ_NANO doubles as this */
#define ABI_ADJ_ADJTIME 0x8000
#define ABI_ADJ_OFFSET_SS_READ 0xa001 /* ADJ_ADJTIME|READONLY|SINGLESHOT */

#define ABI_TIME_OK 0
#define ABI_TIME_ERROR 5

/* include/uapi/linux/timex.h:44-56: STA_PLL is 0x0001; 0x0002 is STA_PPSFREQ.
 * The distinction matters: `ntp_update_offset()` discards an ADJ_OFFSET unless
 * STA_PLL is set (kernel/time/ntp.c:291-292), so a case that sets 0x0002 and
 * then expects the offset to be rendered back can never pass on Linux. */
#define ABI_STA_PLL 0x0001
#define ABI_STA_PPSFREQ 0x0002
#define ABI_STA_UNSYNC 0x0040
#define ABI_STA_NANO 0x2000
#define ABI_STA_MODE 0x4000
#define ABI_STA_CLK 0x8000

#define ABI_NTP_PHASE_LIMIT 16000000
#define ABI_TOLERANCE 32768000
#define ABI_MAX_TAI_OFFSET 100000
/* include/linux/time64.h: TIME_SETTOD_SEC_MAX == KTIME_SEC_MAX - 30 years. */
#define ABI_TIME_SETTOD_SEC_MAX 8277292036LL

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_TIME_ABI_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

#define CASE(case_) puts("THEKERNEL_ABI_CASE " case_ "." KIND)
#define ASSERT(case_, name_) puts("THEKERNEL_ABI_ASSERT " case_ "." KIND " " name_ " pass")
#define RESULT(case_) puts("THEKERNEL_ABI_RESULT " case_ "." KIND " pass")

static long raw_gettimeofday(struct timeval *tv, struct timezone *tz) {
    return syscall(SYS_gettimeofday, tv, tz);
}

static long raw_settimeofday(const struct timeval *tv, const struct timezone *tz) {
    return syscall(SYS_settimeofday, tv, tz);
}

static long raw_clock_settime(clockid_t clock, const struct timespec *ts) {
    return syscall(SYS_clock_settime, clock, ts);
}

static long raw_clock_gettime(clockid_t clock, struct timespec *ts) {
    return syscall(SYS_clock_gettime, clock, ts);
}

static long raw_clock_getres(clockid_t clock, struct timespec *ts) {
    return syscall(SYS_clock_getres, clock, ts);
}

static long raw_adjtimex(struct timex *tx) { return syscall(SYS_adjtimex, tx); }

static long raw_clock_nanosleep(clockid_t clock, int flags, const struct timespec *req) {
    return syscall(SYS_clock_nanosleep, clock, flags, req, NULL);
}

static long raw_clock_adjtime(clockid_t clock, struct timex *tx) {
    return syscall(SYS_clock_adjtime, clock, tx);
}

/* The state a bare read reports, ignoring `time`, which advances between two
 * calls and is compared against the wall clock elsewhere. */
struct timex_state {
    int status;
    long constant, maxerror, esterror, tolerance, tick, precision;
    long offset, freq;
    int tai;
};

static void snapshot_state(const struct timex *tx, struct timex_state *out) {
    out->status = tx->status;
    out->constant = tx->constant;
    out->maxerror = tx->maxerror;
    out->esterror = tx->esterror;
    out->tolerance = tx->tolerance;
    out->tick = tx->tick;
    out->precision = tx->precision;
    out->offset = tx->offset;
    out->freq = tx->freq;
    out->tai = tx->tai;
}

static int same_state(const struct timex_state *a, const struct timex_state *b) {
    return a->status == b->status && a->constant == b->constant &&
           a->maxerror == b->maxerror && a->esterror == b->esterror &&
           a->tolerance == b->tolerance && a->tick == b->tick &&
           a->precision == b->precision && a->offset == b->offset &&
           a->freq == b->freq && a->tai == b->tai;
}

/* Nanoseconds between two samples of the same clock, as a signed count. */
static long realtime_delta(const struct timespec *before, const struct timespec *after) {
    return (after->tv_sec - before->tv_sec) * 1000000000L +
           (after->tv_nsec - before->tv_nsec);
}

/* The `/proc/self/timens_offsets` format is `clockid offset-sec offset-nsec`
 * per line (`timens_offsets_write()`, fs/proc/base.c:1617-1683).  Returns 0 on
 * success. */
static int write_timens_offset(long sec, long nsec) {
    char text[64];
    int length = snprintf(text, sizeof(text), "monotonic %ld %ld\nboottime %ld %ld\n", sec, nsec,
                          sec, nsec);
    if (length <= 0) {
        return -1;
    }
    int fd = open("/proc/self/timens_offsets", O_WRONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t written = write(fd, text, (size_t)length);
    int saved = errno;
    close(fd);
    errno = saved;
    return written == length ? 0 : -1;
}

/* Runs in a forked child.  `unshare(CLONE_NEWTIME)` moves this process into a
 * fresh time namespace whose offsets are still zero (`timens_install()`,
 * kernel/time/namespace.c:176-211); writing `/proc/self/timens_offsets` then
 * changes `time_ns_for_children`, so only the *next* fork enters the warp
 * (`proc_timens_set_offset()`, kernel/time/namespace.c:266-310, applied by
 * `timens_on_fork()`, kernel/time/namespace.c:213-240).  The parent hands this
 * child its own two readings taken immediately before the fork, so the
 * difference across the fork is the offset the child's namespace injected into
 * each clock.  One second is far larger than the few microseconds of drift
 * between the parent's two reads. */
static int check_monotonic_raw_is_namespaced(long monotonic_before, long raw_before) {
    const long offset_ns = 1000000000L;
    const long drift_allowance_ns = 40000000L;

    if (syscall(SYS_unshare, CLONE_NEWTIME) != 0) {
        return fail("monotonic-raw-unshare");
    }
    if (write_timens_offset(offset_ns / 1000000000L, offset_ns % 1000000000L) != 0) {
        return fail("monotonic-raw-offset-write");
    }
    pid_t warped = fork();
    if (warped < 0) {
        return fail("monotonic-raw-warped-fork");
    }
    if (warped == 0) {
        struct timespec monotonic_now, raw_now;
        /* The child never returns into the caller: a failure has to end this
         * process, or the rest of the case would run twice. */
        if (raw_clock_gettime(CLOCK_MONOTONIC, &monotonic_now) != 0 ||
            raw_clock_gettime(CLOCK_MONOTONIC_RAW, &raw_now) != 0) {
            _exit(fail("monotonic-raw-after"));
        }
        long monotonic_shift = (monotonic_now.tv_sec * 1000000000L + monotonic_now.tv_nsec) -
                               monotonic_before;
        long raw_shift = (raw_now.tv_sec * 1000000000L + raw_now.tv_nsec) - raw_before;
        /* Both clocks are read after the fork, so the namespace offset is
         * already inside both; only their *difference* is compared, which
         * cancels the offset and any scheduling delay.  A raw clock that skips
         * `timens_add_monotonic()` reports the offset short by a whole second. */
        long skew = raw_shift - monotonic_shift;
        if (monotonic_shift < offset_ns || raw_shift < offset_ns || skew < -drift_allowance_ns ||
            skew > drift_allowance_ns) {
            errno = EPROTO;
            _exit(fail("monotonic-raw-namespace-offset"));
        }
        _exit(0);
    }
    int warped_status = 0;
    if (waitpid(warped, &warped_status, 0) != warped) {
        return fail("monotonic-raw-warped-wait");
    }
    if (!WIFEXITED(warped_status)) {
        return fail("monotonic-raw-warped-signal");
    }
    return WEXITSTATUS(warped_status);
}

static int case_gettimeofday(void) {
    struct timespec before, after;
    struct timeval tv;
    struct timezone tz;

    CASE("gettimeofday");

    /* gettimeofday(NULL, NULL) is a successful no-op and a filled timeval is
     * bracketed by two CLOCK_REALTIME reads. */
    errno = 0;
    if (raw_gettimeofday(NULL, NULL) != 0) {
        return fail("gettimeofday-null-null");
    }
    memset(&tv, 0xa5, sizeof(tv));
    errno = 0;
    if (raw_gettimeofday(&tv, NULL) != 0 || tv.tv_usec < 0 || tv.tv_usec > 999999 ||
        raw_clock_gettime(CLOCK_REALTIME, &before) != 0 ||
        raw_clock_gettime(CLOCK_REALTIME, &after) != 0) {
        return fail("gettimeofday-bracket");
    }
    if (tv.tv_sec < before.tv_sec - 1 || tv.tv_sec > after.tv_sec + 1) {
        errno = EPROTO;
        return fail("gettimeofday-bracket-value");
    }
    ASSERT("gettimeofday", "READ_BRACKETS_CLOCK_REALTIME");
    ASSERT("gettimeofday", "NULL_ARGUMENTS_ACCEPTED");

    /* settimeofday(NULL, tz) stores the process-wide timezone that
     * gettimeofday(NULL, tz) then reports, and the *first* stored timezone also
     * warps CLOCK_REALTIME by `tz_minuteswest * 60` seconds
     * (do_sys_settimeofday64(), kernel/time/time.c:185-192 ->
     * timekeeping_warp_clock(), kernel/time/timekeeping.c:1781-1791, which
     * injects sys_tz.tz_minuteswest * 60 into the wall clock).  This is the
     * first timezone store in this program, so `firsttime` is still set. */
    memset(&tz, 0, sizeof(tz));
    tz.tz_minuteswest = 120;
    tz.tz_dsttime = 1;
    errno = 0;
    if (raw_clock_gettime(CLOCK_REALTIME, &before) != 0 ||
        raw_settimeofday(NULL, &tz) != 0) {
        return fail("timezone-store");
    }
    memset(&tz, 0, sizeof(tz));
    errno = 0;
    if (raw_gettimeofday(NULL, &tz) != 0 || tz.tz_minuteswest != 120 || tz.tz_dsttime != 1) {
        return fail("timezone-readback");
    }
    if (raw_clock_gettime(CLOCK_REALTIME, &after) != 0) {
        return fail("timezone-store-clock");
    }
    long warp = realtime_delta(&before, &after);
    if (warp < 7200000000000L || warp > 7200000000000L + 2000000000L) {
        errno = EPROTO;
        return fail("timezone-store-warp");
    }
    /* The warp is one-shot: a second store of a different offset must not move
     * the clock again, because `firsttime` was cleared by the first one. */
    memset(&tz, 0, sizeof(tz));
    tz.tz_minuteswest = 60;
    tz.tz_dsttime = 1;
    errno = 0;
    if (raw_clock_gettime(CLOCK_REALTIME, &before) != 0 ||
        raw_settimeofday(NULL, &tz) != 0 ||
        raw_clock_gettime(CLOCK_REALTIME, &after) != 0) {
        return fail("timezone-second-store");
    }
    long second = realtime_delta(&before, &after);
    if (second < 0 || second > 1000000000L) {
        errno = EPROTO;
        return fail("timezone-second-warp");
    }
    /* The retained timezone is left at 60/1 for the settimeofday case, which
     * asserts that a rejected store preserves it and then clears it. */
    ASSERT("gettimeofday", "TIMEZONE_ROUND_TRIP");

    /* CLOCK_MONOTONIC_RAW is a namespaced clock: the monotonic time-namespace
     * offset is added to it exactly as it is to CLOCK_MONOTONIC
     * (`posix_get_monotonic_raw()`, kernel/time/posix-timers.c:222-227, via
     * `timens_add_monotonic()`, include/linux/time_namespace.h:73-78).  Doing
     * this in a forked child keeps the rest of the program in the initial
     * namespace. */
    struct timespec monotonic_before;
    struct timespec raw_before;
    if (raw_clock_gettime(CLOCK_MONOTONIC, &monotonic_before) != 0 ||
        raw_clock_gettime(CLOCK_MONOTONIC_RAW, &raw_before) != 0) {
        return fail("monotonic-raw-before");
    }
    long monotonic_before_ns = monotonic_before.tv_sec * 1000000000L + monotonic_before.tv_nsec;
    long raw_before_ns = raw_before.tv_sec * 1000000000L + raw_before.tv_nsec;
    pid_t child = fork();
    if (child < 0) {
        return fail("monotonic-raw-fork");
    }
    if (child == 0) {
        _exit(check_monotonic_raw_is_namespaced(monotonic_before_ns, raw_before_ns));
    }
    int child_status = 0;
    if (waitpid(child, &child_status, 0) != child) {
        return fail("monotonic-raw-wait");
    }
    if (!WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
        return fail("monotonic-raw-child");
    }
    ASSERT("gettimeofday", "MONOTONIC_RAW_SHARES_MONOTONIC_OFFSET");

    /* A faulting timezone pointer is EFAULT even when the timeval is NULL. */
    errno = 0;
    if (raw_gettimeofday(NULL, (void *)1) != -1 || errno != EFAULT) {
        return fail("gettimeofday-timezone-efault");
    }
    errno = 0;
    if (raw_gettimeofday((void *)1, NULL) != -1 || errno != EFAULT) {
        return fail("gettimeofday-timeval-efault");
    }
    ASSERT("gettimeofday", "TIMEZONE_EFAULT");

    RESULT("gettimeofday");
    return 0;
}

static int case_settimeofday(void) {
    struct timeval tv;
    struct timezone tz;

    CASE("settimeofday");

    /* tv_usec is validated from the first copy-in, before the timezone pointer
     * is read: a malformed sub-second field is EINVAL, not EFAULT. */
    memset(&tv, 0, sizeof(tv));
    tv.tv_usec = 1000000;
    errno = 0;
    if (raw_settimeofday(&tv, (void *)1) != -1 || errno != EINVAL) {
        return fail("settimeofday-usec-before-timezone");
    }
    tv.tv_usec = -1;
    errno = 0;
    if (raw_settimeofday(&tv, (void *)1) != -1 || errno != EINVAL) {
        return fail("settimeofday-negative-usec");
    }
    ASSERT("settimeofday", "SUBSECOND_EINVAL_BEFORE_TIMEZONE");

    /* The seconds are only validated after the timezone copy, so this one is
     * EFAULT even though a negative wall time would never be accepted. */
    memset(&tv, 0, sizeof(tv));
    tv.tv_sec = -5;
    errno = 0;
    if (raw_settimeofday(&tv, (void *)1) != -1 || errno != EFAULT) {
        return fail("settimeofday-timezone-before-seconds");
    }
    errno = 0;
    if (raw_settimeofday(NULL, (void *)1) != -1 || errno != EFAULT) {
        return fail("settimeofday-timezone-efault");
    }
    ASSERT("settimeofday", "TIMEZONE_EFAULT_BEFORE_SECONDS");

    /* The same timeval is only malformed in its sub-second field, so the
     * pairwise comparison is what makes the ordering above meaningful: EINVAL
     * here and EFAULT there can only both hold when `tv_usec` is read and
     * checked before the timezone pointer, which is what
     * `__do_sys_settimeofday()` does (kernel/time/time.c:213-222) before it
     * reaches `do_sys_settimeofday64()` (kernel/time/time.c:169-197). */
    memset(&tv, 0, sizeof(tv));
    tv.tv_sec = -5;
    tv.tv_usec = 1000000;
    errno = 0;
    if (raw_settimeofday(&tv, (void *)1) != -1 || errno != EINVAL) {
        return fail("settimeofday-usec-before-bad-seconds");
    }
    ASSERT("settimeofday", "SUBSECOND_EINVAL_BEFORE_SECONDS");

    /* An out-of-range timezone is rejected without disturbing the stored one,
     * which the preceding case left at 60 minutes west with dst_time 1
     * (`do_sys_settimeofday64()` checks the range before `sys_tz = *tz`). */
    memset(&tz, 0, sizeof(tz));
    tz.tz_minuteswest = 901;
    errno = 0;
    if (raw_settimeofday(NULL, &tz) != -1 || errno != EINVAL) {
        return fail("settimeofday-timezone-range");
    }
    memset(&tz, 0, sizeof(tz));
    if (raw_gettimeofday(NULL, &tz) != 0 || tz.tz_minuteswest != 60 || tz.tz_dsttime != 1) {
        errno = EPROTO;
        return fail("settimeofday-timezone-preserved");
    }
    memset(&tz, 0, sizeof(tz));
    if (raw_settimeofday(NULL, &tz) != 0) {
        return fail("settimeofday-timezone-restore");
    }
    if (raw_gettimeofday(NULL, &tz) != 0 || tz.tz_minuteswest != 0 || tz.tz_dsttime != 0) {
        errno = EPROTO;
        return fail("settimeofday-timezone-restored");
    }
    ASSERT("settimeofday", "TIMEZONE_RANGE_EINVAL_PRESERVES");

    /* timespec64_valid_settod(): the bound itself is already invalid, and a
     * negative wall time is invalid. */
    memset(&tv, 0, sizeof(tv));
    tv.tv_sec = ABI_TIME_SETTOD_SEC_MAX;
    errno = 0;
    if (raw_settimeofday(&tv, NULL) != -1 || errno != EINVAL) {
        return fail("settimeofday-set-to-time-bound");
    }
    tv.tv_sec = -1;
    errno = 0;
    if (raw_settimeofday(&tv, NULL) != -1 || errno != EINVAL) {
        return fail("settimeofday-negative-seconds");
    }
    ASSERT("settimeofday", "SET_TO_TIME_BOUND_EINVAL");

    /* A valid wall time that precedes CLOCK_MONOTONIC is rejected. */
    memset(&tv, 0, sizeof(tv));
    errno = 0;
    if (raw_settimeofday(&tv, NULL) != -1 || errno != EINVAL) {
        return fail("settimeofday-before-monotonic");
    }
    ASSERT("settimeofday", "BEFORE_MONOTONIC_EINVAL");

    RESULT("settimeofday");
    return 0;
}

static int case_clock_settime(void) {
    struct timespec ts = {0};
    int self_pid = (int)getpid();

    CASE("clock_settime");

    /* Only CLOCK_REALTIME has a setter: every other clock, including the
     * wake-alarm clock and a CLOCKFD-encoded id, is EINVAL. */
    errno = 0;
    if (raw_clock_settime(CLOCK_MONOTONIC, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-monotonic");
    }
    errno = 0;
    if (raw_clock_settime(CLOCK_REALTIME_ALARM, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-realtime-alarm");
    }
    errno = 0;
    if (raw_clock_settime(CLOCK_TAI, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-tai");
    }
    errno = 0;
    if (raw_clock_settime(CLOCK_THREAD_CPUTIME_ID, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-thread-cputime");
    }
    errno = 0;
    if (raw_clock_settime(CLOCK_PROCESS_CPUTIME_ID, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-process-cputime");
    }
    /* A CLOCKFD-encoded id names a dynamic POSIX clock: clock_settime(2) finds
     * no usable descriptor and reports EINVAL, and it does so before any
     * capability test. */
    errno = 0;
    if (raw_clock_settime((clockid_t)ABI_CLOCKFD_ID(987), &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-clockfd");
    }
    /* An unknown positive id is not a clock at all. */
    errno = 0;
    if (raw_clock_settime((clockid_t)7899, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-unknown");
    }
    ASSERT("clock_settime", "CLOCK_CLASSIFICATION_EINVAL");

    errno = 0;
    if (raw_clock_settime(CLOCK_REALTIME, (void *)1) != -1 || errno != EFAULT) {
        return fail("clock_settime-efault");
    }
    ASSERT("clock_settime", "COPY_IN_EFAULT");

    /* A negative (encoded) clock id is *not* rejected before the copy: Linux
     * routes it to clock_posix_cpu, whose table does carry a `.clock_set`
     * (`clockid_to_kclock()`, kernel/time/posix-timers.c:1541-1554;
     * `clock_posix_cpu`, kernel/time/posix-cpu-timers.c:1706-1718), so the
     * timespec is copied in and then `posix_cpu_clock_set()` refuses.  The
     * faulting pointer therefore reports EFAULT, and a readable timespec
     * reports EPERM -- never EINVAL -- for a clock whose encoded pid exists
     * (`kernel/time/posix-cpu-timers.c:180-189`). */
    const clockid_t cpu_clocks[] = {
        (clockid_t)ABI_CPUCLOCK_PROCESS(self_pid, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_PROCESS(self_pid, ABI_CPUCLOCK_SCHED),
        (clockid_t)ABI_CPUCLOCK_THREAD((int)syscall(SYS_gettid), ABI_CPUCLOCK_SCHED),
    };
    for (unsigned i = 0; i < sizeof(cpu_clocks) / sizeof(cpu_clocks[0]); i++) {
        errno = 0;
        if (raw_clock_settime(cpu_clocks[i], (void *)1) != -1 || errno != EFAULT) {
            return fail("clock_settime-cpu-efault");
        }
        errno = 0;
        if (raw_clock_settime(cpu_clocks[i], &ts) != -1 || errno != EPERM) {
            return fail("clock_settime-cpu-eperm");
        }
    }
    /* A pid that names no task is EINVAL, and that lookup happens after the
     * copy-in as well. */
    errno = 0;
    if (raw_clock_settime((clockid_t)ABI_CPUCLOCK_PROCESS(999999, ABI_CPUCLOCK_PROF), &ts) != -1 ||
        errno != EINVAL) {
        return fail("clock_settime-cpu-stale-pid");
    }
    ASSERT("clock_settime", "CPU_CLOCK_EPERM_AFTER_COPY");

    ts.tv_sec = ABI_TIME_SETTOD_SEC_MAX;
    errno = 0;
    if (raw_clock_settime(CLOCK_REALTIME, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-set-to-time-bound");
    }
    ts.tv_sec = -1;
    errno = 0;
    if (raw_clock_settime(CLOCK_REALTIME, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-negative");
    }
    ASSERT("clock_settime", "SET_TO_TIME_BOUND_EINVAL");

    ts.tv_sec = 0;
    ts.tv_nsec = 0;
    errno = 0;
    if (raw_clock_settime(CLOCK_REALTIME, &ts) != -1 || errno != EINVAL) {
        return fail("clock_settime-before-monotonic");
    }
    ASSERT("clock_settime", "BEFORE_MONOTONIC_EINVAL");

    RESULT("clock_settime");
    return 0;
}

static int case_adjtimex(void) {
    struct timex tx;
    struct timespec before, after, realtime, tai;
    long result;

    CASE("adjtimex");

    /* A bare read reports ntp_init()'s state, never writes `modes` back, and
     * reports TIME_ERROR because STA_UNSYNC is set. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = 0;
    if (raw_clock_gettime(CLOCK_REALTIME, &before) != 0) {
        return fail("adjtimex-bare-clock");
    }
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR) {
        errno = EPROTO;
        return fail("adjtimex-bare-result");
    }
    if (tx.modes != 0 || tx.status != ABI_STA_UNSYNC || tx.constant != 2 ||
        tx.maxerror != ABI_NTP_PHASE_LIMIT || tx.esterror != ABI_NTP_PHASE_LIMIT ||
        tx.tolerance != ABI_TOLERANCE || tx.tick != 10000 || tx.precision != 1 ||
        tx.tai != 0 || tx.offset != 0 || tx.freq != 0) {
        errno = EPROTO;
        return fail("adjtimex-bare-state");
    }
    /* STA_NANO is clear, so `time` carries microseconds and the seconds
     * bracket the wall clock. */
    if (tx.time.tv_usec < 0 || tx.time.tv_usec > 999999 ||
        tx.time.tv_sec < before.tv_sec - 1 || tx.time.tv_sec > before.tv_sec + 1) {
        errno = EPROTO;
        return fail("adjtimex-bare-time");
    }
    ASSERT("adjtimex", "BARE_READ_INITIAL_STATE");

    /* Unknown mode bits are ignored but echoed, because Linux never writes
     * `modes`. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = 0x00010000u | ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR ||
        tx.modes != (0x00010000u | ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT)) {
        errno = EPROTO;
        return fail("adjtimex-unknown-mode-bits");
    }
    ASSERT("adjtimex", "UNKNOWN_MODE_BITS_ECHOED");

    /* ADJ_ADJTIME is only legal together with the singleshot bit. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-adjtime-without-singleshot");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME | ABI_ADJ_NANO;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-adjtime-nano-without-singleshot");
    }
    ASSERT("adjtimex", "ADJ_ADJTIME_REQUIRES_SINGLESHOT");

    /* adjtime(3) semantics: the residual being replaced is returned and the
     * new value is stored, without touching the NTP offset. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT;
    tx.offset = 500000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || tx.offset != 0) {
        errno = EPROTO;
        return fail("adjtimex-singleshot-first");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT;
    tx.offset = 0;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || tx.offset != 500000) {
        errno = EPROTO;
        return fail("adjtimex-singleshot-previous");
    }
    ASSERT("adjtimex", "SINGLESHOT_RETURNS_PREVIOUS");

    /* ADJ_OFFSET_SS_READ reports the residual without writing it. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET_SS_READ;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || tx.offset != 0) {
        errno = EPROTO;
        return fail("adjtimex-ss-read");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT;
    tx.offset = -250000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || tx.offset != 0) {
        errno = EPROTO;
        return fail("adjtimex-ss-read-arm");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET_SS_READ;
    tx.offset = 12345;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || tx.offset != -250000) {
        errno = EPROTO;
        return fail("adjtimex-ss-read-residual");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_ADJTIME | ABI_ADJ_OFFSET_SINGLESHOT;
    tx.offset = 0;
    if (raw_adjtimex(&tx) != ABI_TIME_ERROR || tx.offset != -250000) {
        errno = EPROTO;
        return fail("adjtimex-ss-read-clear");
    }
    ASSERT("adjtimex", "SS_READ_REPORTS_RESIDUAL");

    /* ADJ_STATUS takes non-read-only bits from the request and keeps only
     * read-only bits from the previous status, so STA_MODE/STA_NANO/STA_CLK
     * cannot be written and STA_UNSYNC can be cleared. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_STATUS;
    tx.status = ABI_STA_MODE | ABI_STA_NANO | ABI_STA_CLK | ABI_STA_PLL;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || (tx.status & (ABI_STA_MODE | ABI_STA_NANO | ABI_STA_CLK)) != 0 ||
        (tx.status & ABI_STA_PLL) == 0 || (tx.status & ABI_STA_UNSYNC) != 0) {
        errno = EPROTO;
        return fail("adjtimex-status-readonly");
    }
    memset(&tx, 0, sizeof(tx));
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.status != ABI_STA_PLL) {
        errno = EPROTO;
        return fail("adjtimex-status-latched");
    }
    ASSERT("adjtimex", "STATUS_READONLY_BITS_DROPPED");

    /* ADJ_NANO selects nanoseconds, ADJ_MICRO selects microseconds, and
     * ADJ_MICRO wins when both are present.  Both offsets below survive the
     * scaled-offset round trip exactly for every supported HZ: 600000 ns in
     * nanosecond units, and 600000 us in microsecond units, which the
     * MAXPHASE clamp then holds at 500000000 ns (500000 us, allowing one
     * microsecond of scaling truncation). */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET | ABI_ADJ_NANO;
    tx.offset = 600000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || (tx.status & ABI_STA_NANO) == 0 || tx.offset != 600000) {
        errno = EPROTO;
        return fail("adjtimex-nano-offset");
    }
    ASSERT("adjtimex", "NANO_RESOLUTION_RENDER");

    /* The residual is stored scaled by NTP_INTERVAL_FREQ, which
     * include/linux/timex.h:151 defines as (HZ), and read back through the
     * inverse pair `(offset << 32) / HZ` then `* HZ >> 32`
     * (kernel/time/ntp.c:331,806).  The truncating division makes a phase that
     * is not a multiple of the tick rate render at most one nanosecond low --
     * at HZ=1000 exactly one low for 25, which is the shape this asserts --
     * while a phase that is a multiple round trips exactly.  Scaling by the
     * ABI's USER_HZ instead, or by any other constant, is invisible *only*
     * because both halves use the same one; what this pair pins is that the
     * guest agrees with Linux's own truncation at its own HZ. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET | ABI_ADJ_NANO;
    tx.offset = 25;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.offset < 24 || tx.offset > 25) {
        errno = EPROTO;
        return fail("adjtimex-nano-offset-subtick");
    }
    /* A multiple of every supported HZ is exact, which bounds the same
     * truncation from the other side. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET | ABI_ADJ_NANO;
    tx.offset = 3000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.offset != 3000) {
        errno = EPROTO;
        return fail("adjtimex-nano-offset-tick-multiple");
    }
    ASSERT("adjtimex", "OFFSET_TRUNCATION_BOUNDED_BY_ONE_NANOSECOND");

    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET | ABI_ADJ_MICRO;
    tx.offset = 600000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || (tx.status & ABI_STA_NANO) != 0 ||
        tx.offset < 499000 || tx.offset > 500000 || tx.time.tv_usec < 0 ||
        tx.time.tv_usec > 999999) {
        errno = EPROTO;
        return fail("adjtimex-micro-offset");
    }
    ASSERT("adjtimex", "MICRO_RESOLUTION_CLAMPS");

    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_OFFSET | ABI_ADJ_NANO | ABI_ADJ_MICRO;
    tx.offset = 600000;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || (tx.status & ABI_STA_NANO) != 0 || tx.offset > 500000) {
        errno = EPROTO;
        return fail("adjtimex-micro-wins");
    }
    ASSERT("adjtimex", "MICRO_WINS_OVER_NANO");

    /* ADJ_TAI publishes the TAI offset that CLOCK_TAI applies, ignores
     * out-of-range values, and can be reset. */
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_TAI;
    tx.constant = 37;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.tai != 37) {
        errno = EPROTO;
        return fail("adjtimex-tai-set");
    }
    if (raw_clock_gettime(CLOCK_TAI, &tai) != 0 || raw_clock_gettime(CLOCK_REALTIME, &realtime) != 0) {
        return fail("adjtimex-tai-clock");
    }
    long leap = (tai.tv_sec - realtime.tv_sec) * 1000000000L + (tai.tv_nsec - realtime.tv_nsec);
    if (leap < 37000000000L - 5000000L || leap > 37000000000L + 5000000L) {
        errno = EPROTO;
        return fail("adjtimex-tai-applied");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_TAI;
    tx.constant = ABI_MAX_TAI_OFFSET + 1;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.tai != 37) {
        errno = EPROTO;
        return fail("adjtimex-tai-range-ignored");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_TAI;
    tx.constant = 0;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_OK || tx.tai != 0) {
        errno = EPROTO;
        return fail("adjtimex-tai-reset");
    }
    if (raw_clock_gettime(CLOCK_TAI, &tai) != 0 || raw_clock_gettime(CLOCK_REALTIME, &realtime) != 0) {
        return fail("adjtimex-tai-reset-clock");
    }
    leap = (tai.tv_sec - realtime.tv_sec) * 1000000000L + (tai.tv_nsec - realtime.tv_nsec);
    if (leap < -5000000L || leap > 5000000L) {
        errno = EPROTO;
        return fail("adjtimex-tai-reset-applied");
    }
    ASSERT("adjtimex", "TAI_OFFSET_READBACK");

    /* ADJ_SETOFFSET injects a wall-clock discontinuity and reports the
     * pre-injection sample in `time`.  The injection publishes the shadow
     * timekeeper with `TK_UPDATE_ALL`, whose `TK_CLEAR_NTP` bit runs
     * `ntp_clear()` (kernel/time/timekeeping.c:31-34 and 1742,
     * kernel/time/ntp.c:334-352), so the clock is left unsynchronised: the
     * call reports `TIME_ERROR`, not `TIME_OK`, and the returned status has
     * STA_UNSYNC set and STA_NANO clear (STA_NANO was cleared by ADJ_MICRO and
     * `ntp_clear()` only adds STA_UNSYNC). */
    if (raw_clock_gettime(CLOCK_REALTIME, &before) != 0) {
        return fail("adjtimex-setoffset-clock");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_SETOFFSET | ABI_ADJ_MICRO;
    tx.time.tv_sec = 1;
    tx.time.tv_usec = 0;
    errno = 0;
    result = raw_adjtimex(&tx);
    if (result != ABI_TIME_ERROR || (tx.status & ABI_STA_UNSYNC) == 0 ||
        (tx.status & ABI_STA_NANO) != 0 || raw_clock_gettime(CLOCK_REALTIME, &after) != 0) {
        errno = EPROTO;
        return fail("adjtimex-setoffset-result");
    }
    long jump = realtime_delta(&before, &after);
    if (jump < 1000000000L || jump > 2000000000L) {
        errno = EPROTO;
        return fail("adjtimex-setoffset-applied");
    }
    if (tx.time.tv_usec < 0 || tx.time.tv_usec > 999999 || tx.time.tv_sec > after.tv_sec ||
        tx.time.tv_sec < before.tv_sec - 1 || tx.time.tv_sec > before.tv_sec + 1) {
        errno = EPROTO;
        return fail("adjtimex-setoffset-sampled-before");
    }
    ASSERT("adjtimex", "SETOFFSET_DISCONTINUITY");

    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_SETOFFSET | ABI_ADJ_MICRO;
    tx.time.tv_usec = 1000000;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-setoffset-usec-bound");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_SETOFFSET | ABI_ADJ_MICRO;
    tx.time.tv_usec = -1;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-setoffset-negative-usec");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_SETOFFSET | ABI_ADJ_MICRO;
    tx.time.tv_sec = ABI_TIME_SETTOD_SEC_MAX;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-setoffset-bound");
    }
    /* The injection may not push CLOCK_REALTIME below CLOCK_MONOTONIC:
     * `timespec64_compare(&tks->wall_to_monotonic, ts) > 0` compares the delta
     * against `wall_to_monotonic`, which is `CLOCK_MONOTONIC - CLOCK_REALTIME`,
     * not against the uptime (kernel/time/timekeeping.c:1714-1721).  The floor
     * is therefore measured here, and the delta is one second past it while the
     * resulting wall time stays positive, so only the monotonic guard can
     * reject this call. */
    struct timespec wall_now;
    struct timespec monotonic_now;
    if (raw_clock_gettime(CLOCK_REALTIME, &wall_now) != 0 ||
        raw_clock_gettime(CLOCK_MONOTONIC, &monotonic_now) != 0) {
        return fail("adjtimex-setoffset-clocks");
    }
    if (wall_now.tv_sec <= monotonic_now.tv_sec) {
        return fail("adjtimex-setoffset-clock-order");
    }
    memset(&tx, 0, sizeof(tx));
    tx.modes = ABI_ADJ_SETOFFSET | ABI_ADJ_MICRO;
    tx.time.tv_sec = -(wall_now.tv_sec - monotonic_now.tv_sec) - 1;
    errno = 0;
    if (raw_adjtimex(&tx) != -1 || errno != EINVAL) {
        return fail("adjtimex-setoffset-before-monotonic");
    }
    ASSERT("adjtimex", "SETOFFSET_VALIDATION");

    RESULT("adjtimex");
    return 0;
}

static void on_expire(int signo) { (void)signo; }

static int case_clock_getres(void) {
    struct timespec ts;
    int tid = (int)syscall(SYS_gettid);

    CASE("clock_getres");

    /* The scheduler clock reports one nanosecond, for the plain clock ids and
     * for their CPUCLOCK_SCHED encodings alike, on every Linux HZ. */
    const clockid_t scheduler[] = {
        CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID,
        (clockid_t)ABI_CPUCLOCK_PROCESS(0, ABI_CPUCLOCK_SCHED),
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_SCHED),
    };
    for (unsigned i = 0; i < sizeof(scheduler) / sizeof(scheduler[0]); i++) {
        ts.tv_sec = -1;
        ts.tv_nsec = -1;
        errno = 0;
        if (raw_clock_getres(scheduler[i], &ts) != 0 || ts.tv_sec != 0 || ts.tv_nsec != 1) {
            return fail("clock_getres-scheduler");
        }
    }
    ASSERT("clock_getres", "SCHEDULER_CLOCKS_ARE_NANOSECOND");

    /* The PROF and VIRT CPU clocks are charged once per accounting tick, so
     * their resolution is (NSEC_PER_SEC + HZ - 1) / HZ: strictly more than one
     * nanosecond for every HZ this kernel supports. */
    const clockid_t accounting[] = {
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_VIRT),
        (clockid_t)ABI_CPUCLOCK_PROCESS(0, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_PROCESS(0, ABI_CPUCLOCK_VIRT),
    };
    for (unsigned i = 0; i < sizeof(accounting) / sizeof(accounting[0]); i++) {
        ts.tv_sec = -1;
        ts.tv_nsec = -1;
        errno = 0;
        if (raw_clock_getres(accounting[i], &ts) != 0 || ts.tv_sec != 0 || ts.tv_nsec <= 1) {
            return fail("clock_getres-accounting");
        }
    }
    ASSERT("clock_getres", "ACCOUNTING_CLOCKS_EXCEED_NANOSECOND");

    /* A null resolution pointer is accepted, and an unknown clock is EINVAL. */
    errno = 0;
    if (raw_clock_getres(CLOCK_MONOTONIC, NULL) != 0) {
        return fail("clock_getres-null");
    }
    errno = 0;
    if (raw_clock_getres((clockid_t)7899, &ts) != -1 || errno != EINVAL) {
        return fail("clock_getres-unknown");
    }
    ASSERT("clock_getres", "NULL_AND_UNKNOWN_IDS");

    /* A CPU clock's encoded pid is resolved before its resolution is
     * reported: `posix_cpu_clock_getres()` starts with
     * `validate_clock_permissions()` -> `pid_for_clock(clock, false)`
     * (kernel/time/posix-cpu-timers.c:97-106,57-105), so a pid that names no
     * task is EINVAL and the caller's own tid is accepted.  For `clock_getres`
     * a thread-group leader is required, which the caller's own tid satisfies.
     */
    const clockid_t live[] = {
        (clockid_t)ABI_CPUCLOCK_PROCESS(0, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_SCHED),
    };
    for (unsigned i = 0; i < sizeof(live) / sizeof(live[0]); i++) {
        ts.tv_sec = -1;
        ts.tv_nsec = -1;
        errno = 0;
        if (raw_clock_getres(live[i], &ts) != 0 || ts.tv_sec != 0 || ts.tv_nsec < 1) {
            return fail("clock_getres-live-pid");
        }
    }
    const clockid_t stale[] = {
        (clockid_t)ABI_CPUCLOCK_PROCESS(999999, ABI_CPUCLOCK_PROF),
        (clockid_t)ABI_CPUCLOCK_PROCESS(999999, ABI_CPUCLOCK_SCHED),
        (clockid_t)ABI_CPUCLOCK_THREAD(999999, ABI_CPUCLOCK_SCHED),
    };
    for (unsigned i = 0; i < sizeof(stale) / sizeof(stale[0]); i++) {
        ts.tv_sec = -1;
        ts.tv_nsec = -1;
        errno = 0;
        if (raw_clock_getres(stale[i], &ts) != -1 || errno != EINVAL ||
            ts.tv_sec != -1 || ts.tv_nsec != -1) {
            return fail("clock_getres-stale-pid");
        }
    }
    ASSERT("clock_getres", "PID_RESOLUTION_BEFORE_RESOLUTION_VALUE");

    RESULT("clock_getres");
    return 0;
}

static int case_clock_nanosleep(void) {
    const struct timespec zero = {0, 0};

    CASE("clock_nanosleep");

    /* A clock whose k_clock table has no nsleep operation is EOPNOTSUPP before
     * the timespec is copied, so a faulting pointer still reports that errno:
     * CLOCK_MONOTONIC_RAW, both *_COARSE clocks and CLOCK_THREAD_CPUTIME_ID,
     * whose clock_thread table omits nsleep
     * (kernel/time/posix-timers.c:1383-1410, posix-cpu-timers.c:1720). */
    const clockid_t unsupported[] = {
        CLOCK_MONOTONIC_RAW,
        CLOCK_REALTIME_COARSE,
        CLOCK_MONOTONIC_COARSE,
        CLOCK_THREAD_CPUTIME_ID,
    };
    for (unsigned i = 0; i < sizeof(unsupported) / sizeof(unsupported[0]); i++) {
        errno = 0;
        if (raw_clock_nanosleep(unsupported[i], 0, (const struct timespec *)1) != -1 ||
            errno != EOPNOTSUPP) {
            return fail("clock_nanosleep-unsupported");
        }
    }
    ASSERT("clock_nanosleep", "UNSUPPORTED_CLOCKS_EOPNOTSUPP_BEFORE_COPY");

    /* An unknown clock id is EINVAL at the same point. */
    errno = 0;
    if (raw_clock_nanosleep((clockid_t)7899, 0, (const struct timespec *)1) != -1 ||
        errno != EINVAL) {
        return fail("clock_nanosleep-unknown");
    }
    ASSERT("clock_nanosleep", "UNKNOWN_CLOCK_EINVAL");

    /* A supported clock copies the interval first, so a faulting pointer is
     * EFAULT rather than EINVAL (kernel/time/posix-timers.c:1394-1399). */
    errno = 0;
    if (raw_clock_nanosleep(CLOCK_MONOTONIC, 0, (const struct timespec *)1) != -1 ||
        errno != EFAULT) {
        return fail("clock_nanosleep-monotonic-efault");
    }
    /* A zero relative interval completes immediately. */
    errno = 0;
    if (raw_clock_nanosleep(CLOCK_MONOTONIC, 0, &zero) != 0) {
        return fail("clock_nanosleep-monotonic-zero");
    }
    ASSERT("clock_nanosleep", "SUPPORTED_CLOCK_COPY_BEFORE_VALIDATION");

    /* Linux never rejects an unknown flag bit on a non-alarm clock: the
     * per-clock `nsleep` reads the single `TIMER_ABSTIME` bit
     * (`common_nsleep()`, kernel/time/posix-timers.c:1355-1363) and the syscall
     * itself only tests that bit to drop `rmtp`
     * (kernel/time/posix-timers.c:1400-1401).  A zero relative interval with
     * every other bit set therefore still completes immediately. */
    errno = 0;
    if (raw_clock_nanosleep(CLOCK_MONOTONIC, 0x8000, &zero) != 0) {
        return fail("clock_nanosleep-monotonic-unknown-flag");
    }
    errno = 0;
    if (raw_clock_nanosleep(CLOCK_REALTIME, 0x100, &zero) != 0) {
        return fail("clock_nanosleep-realtime-unknown-flag");
    }
    ASSERT("clock_nanosleep", "UNKNOWN_FLAGS_IGNORED");

    /* The alarm clocks are the one exception: `alarm_timer_nsleep()` masks the
     * flags itself, after the RTC test and before the capability test
     * (kernel/time/posix-timers.c is not involved; kernel/time/alarmtimer.c:775-782).
     * The oracle has no RTC driver for this platform, so EOPNOTSUPP is the
     * outcome there; accept the three documented outcomes and never a fourth. */
    errno = 0;
    long alarm_sleep = raw_clock_nanosleep(CLOCK_REALTIME_ALARM, 0x8000, &zero);
    if (alarm_sleep != 0 && errno != EOPNOTSUPP && errno != EPERM && errno != EINVAL) {
        return fail("clock_nanosleep-alarm-unknown-flag");
    }
    errno = 0;
    long alarm_sleep_ok = raw_clock_nanosleep(CLOCK_REALTIME_ALARM, 0, &zero);
    if (alarm_sleep_ok != 0 && errno != EOPNOTSUPP && errno != EPERM) {
        return fail("clock_nanosleep-alarm-plain");
    }
    /* The order *within* the alarm admission is only visible without privilege,
     * which the last case covers; here the documented outcome set is pinned so
     * that "no RTC" cannot silently become "some other errno". */
    ASSERT("clock_nanosleep", "ALARM_OUTCOME_MATCHES_CONFIG");

    /* A per-thread CPU clock can never wait for the calling thread's own CPU
     * time, which Linux diagnoses before any permission check
     * (kernel/time/posix-cpu-timers.c:1630-1638). */
    int tid = (int)syscall(SYS_gettid);
    const clockid_t self_thread[] = {
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_SCHED),
        (clockid_t)ABI_CPUCLOCK_THREAD(0, ABI_CPUCLOCK_SCHED),
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_PROF),
    };
    for (unsigned i = 0; i < sizeof(self_thread) / sizeof(self_thread[0]); i++) {
        errno = 0;
        if (raw_clock_nanosleep(self_thread[i], 0, &zero) != -1 || errno != EINVAL) {
            return fail("clock_nanosleep-self-thread");
        }
    }
    ASSERT("clock_nanosleep", "ENCODED_SELF_THREAD_IS_EINVAL");

    /* A process CPU clock is supported and a zero interval completes
     * immediately, on the process accounting clock rather than the wall
     * clock. */
    errno = 0;
    if (raw_clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0, &zero) != 0) {
        return fail("clock_nanosleep-process-cputime-zero");
    }
    ASSERT("clock_nanosleep", "PROCESS_CPUTIME_ZERO_INTERVAL");

    RESULT("clock_nanosleep");
    return 0;
}

static int case_clock_adjtime(void) {
    struct timex adjtimex_tx, adjtimex_tx_again, tx;
    struct timex_state from_adjtimex, from_clock_adjtime, after_read;
    long adjtimex_result, result;

    CASE("clock_adjtime");

    /* Both syscalls run do_adjtimex(), so a bare read through either one
     * reports the same state, and that state is not modified by reading it. */
    memset(&adjtimex_tx, 0, sizeof(adjtimex_tx));
    errno = 0;
    adjtimex_result = raw_adjtimex(&adjtimex_tx);
    if (adjtimex_result < 0) {
        return fail("clock_adjtime-adjtimex-read");
    }
    snapshot_state(&adjtimex_tx, &from_adjtimex);
    memset(&tx, 0, sizeof(tx));
    errno = 0;
    result = raw_clock_adjtime(CLOCK_REALTIME, &tx);
    if (result != adjtimex_result) {
        return fail("clock_adjtime-realtime-result");
    }
    snapshot_state(&tx, &from_clock_adjtime);
    if (!same_state(&from_adjtimex, &from_clock_adjtime)) {
        errno = EPROTO;
        return fail("clock_adjtime-realtime-state");
    }
    /* The successful call copies the timex back, and `modes` still holds the
     * caller's zero because Linux never writes it. */
    if (tx.modes != 0) {
        errno = EPROTO;
        return fail("clock_adjtime-realtime-copyout");
    }
    memset(&adjtimex_tx_again, 0, sizeof(adjtimex_tx_again));
    errno = 0;
    if (raw_adjtimex(&adjtimex_tx_again) != adjtimex_result) {
        return fail("clock_adjtime-read-result");
    }
    snapshot_state(&adjtimex_tx_again, &after_read);
    if (!same_state(&from_adjtimex, &after_read)) {
        errno = EPROTO;
        return fail("clock_adjtime-read-state");
    }
    ASSERT("clock_adjtime", "REALTIME_SHARES_ADJTIMEX_CORE");

    /* Every other clock in the table lacks a clock_adj operation, which Linux
     * reports as EOPNOTSUPP rather than EINVAL, including the CPU clocks that
     * are routed to clock_posix_cpu
     * (kernel/time/posix-timers.c:1160-1171). */
    int tid = (int)syscall(SYS_gettid);
    const clockid_t unsupported[] = {
        CLOCK_MONOTONIC,
        CLOCK_BOOTTIME,
        CLOCK_TAI,
        CLOCK_REALTIME_ALARM,
        CLOCK_MONOTONIC_RAW,
        CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID,
        (clockid_t)ABI_CPUCLOCK_THREAD(tid, ABI_CPUCLOCK_SCHED),
    };
    for (unsigned i = 0; i < sizeof(unsupported) / sizeof(unsupported[0]); i++) {
        memset(&tx, 0, sizeof(tx));
        errno = 0;
        if (raw_clock_adjtime(unsupported[i], &tx) != -1 || errno != EOPNOTSUPP) {
            return fail("clock_adjtime-unsupported-clock");
        }
    }
    ASSERT("clock_adjtime", "CLOCKS_WITHOUT_CLOCK_ADJ_ARE_EOPNOTSUPP");

    /* An id that names no clock at all is EINVAL. */
    memset(&tx, 0, sizeof(tx));
    errno = 0;
    if (raw_clock_adjtime((clockid_t)7899, &tx) != -1 || errno != EINVAL) {
        return fail("clock_adjtime-unknown-clock");
    }
    ASSERT("clock_adjtime", "UNKNOWN_CLOCK_IS_EINVAL");

    /* The timex is copied in before the clock is classified, so a faulting
     * pointer is EFAULT even for a clock that would be rejected
     * (kernel/time/posix-timers.c:1178-1181). */
    const clockid_t faulting[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, 7899};
    for (unsigned i = 0; i < sizeof(faulting) / sizeof(faulting[0]); i++) {
        errno = 0;
        if (raw_clock_adjtime(faulting[i], (struct timex *)1) != -1 || errno != EFAULT) {
            return fail("clock_adjtime-faulting-pointer");
        }
    }
    ASSERT("clock_adjtime", "COPY_IN_PRECEDES_CLASSIFICATION");

    RESULT("clock_adjtime");
    return 0;
}

static int case_timer_create(void) {
    struct sigevent event;
    timer_t id = (timer_t)-1;

    CASE("timer_create");

    /* The identifier is copied out before the clock-specific admission runs,
     * so a faulting output pointer is EFAULT even for an alarm clock that the
     * kernel may refuse afterwards. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    errno = 0;
    if (syscall(SYS_timer_create, CLOCK_REALTIME_ALARM, &event, (void *)1) != -1 ||
        errno != EFAULT) {
        return fail("timer_create-alarm-efault");
    }
    ASSERT("timer_create", "ALARM_COPYOUT_BEFORE_ADMISSION");

    /* A clock without timer support is EOPNOTSUPP. */
    errno = 0;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC_RAW, &event, &id) != -1 || errno != EOPNOTSUPP) {
        return fail("timer_create-raw");
    }
    errno = 0;
    if (syscall(SYS_timer_create, 123456, &event, &id) != -1 || errno != EINVAL) {
        return fail("timer_create-bad-clock");
    }
    ASSERT("timer_create", "CLOCK_CLASSIFICATION_ERRNOS");

    /* SIGEV_THREAD is a kernel-accepted notification mode: the kernel only
     * validates the signal number and the C library runs the callback. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_THREAD;
    event.sigev_signo = SIGALRM;
    event.sigev_value.sival_ptr = (void *)&event;
    errno = 0;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) {
        return fail("timer_create-sigev-thread");
    }
    errno = 0;
    if (syscall(SYS_timer_delete, id) != 0) {
        return fail("timer_delete-sigev-thread");
    }

    ASSERT("timer_create", "SIGEV_THREAD_ACCEPTED");

    /* A wake-alarm clock only exists when the kernel has an RTC and only the
     * holder of CAP_WAKE_ALARM may create it, and the first is a property of
     * the oracle kernel build.  Accept every documented outcome: a usable
     * timer, EOPNOTSUPP before the capability test when no RTC is present, or
     * EPERM when the caller has no CAP_WAKE_ALARM (both guests run this case as
     * root, so EPERM is only observable when it is replayed unprivileged).
     * The admission *order* is asserted above and does not depend on either
     * fact. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    errno = 0;
    long created = syscall(SYS_timer_create, CLOCK_REALTIME_ALARM, &event, &id);
    if (created == 0) {
        if (syscall(SYS_timer_delete, id) != 0) {
            return fail("timer_delete-alarm");
        }
    } else if (errno != EOPNOTSUPP && errno != EPERM) {
        return fail("timer_create-alarm-errno");
    }
    ASSERT("timer_create", "ALARM_CLOCK_OUTCOME_DOCUMENTED");

    RESULT("timer_create");
    return 0;
}

static int case_timer_getoverrun(void) {
    struct sigevent event;
    timer_t id = (timer_t)-1;

    CASE("timer_getoverrun");

    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) {
        return fail("timer_getoverrun-create");
    }
    errno = 0;
    if (syscall(SYS_timer_getoverrun, id) != 0) {
        return fail("timer_getoverrun-fresh");
    }
    if (syscall(SYS_timer_delete, id) != 0) {
        return fail("timer_getoverrun-delete");
    }
    errno = 0;
    if (syscall(SYS_timer_getoverrun, id) != -1 || errno != EINVAL) {
        return fail("timer_getoverrun-deleted");
    }
    errno = 0;
    if (syscall(SYS_timer_getoverrun, 0x7fffffff) != -1 || errno != EINVAL) {
        return fail("timer_getoverrun-invalid");
    }
    ASSERT("timer_getoverrun", "ZERO_BEFORE_FIRST_SIGNAL");
    ASSERT("timer_getoverrun", "INVALID_ID_EINVAL");

    /* Linux reports `it_overrun_last`, which is latched only where a queued
     * notification actually reaches the task
     * (`__posixtimer_deliver_signal()`, kernel/time/posix-timers.c:298-327);
     * the accumulating `it_overrun` that a pending notification carries is
     * reported through `si_overrun` instead, never through this syscall
     * (`timer_overrun_to_int()`, kernel/time/posix-timers.c:283-289).  A
     * SIGEV_NONE timer has no notification to latch, so even after many
     * expirations of a fast periodic timer the syscall must keep returning
     * zero; a counter that accumulated overruns would return roughly fifty
     * here. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    id = (timer_t)-1;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &id) != 0) {
        return fail("timer_getoverrun-periodic-create");
    }
    const struct itimerspec periodic = {
        .it_interval = {.tv_sec = 0, .tv_nsec = 1000000},
        .it_value = {.tv_sec = 0, .tv_nsec = 1000000},
    };
    if (syscall(SYS_timer_settime, id, 0, &periodic, NULL) != 0) {
        return fail("timer_getoverrun-periodic-arm");
    }
    const struct timespec settle = {.tv_sec = 0, .tv_nsec = 100000000};
    if (syscall(SYS_clock_nanosleep, CLOCK_MONOTONIC, 0, &settle, NULL) != 0) {
        return fail("timer_getoverrun-periodic-sleep");
    }
    errno = 0;
    long expired_overrun = syscall(SYS_timer_getoverrun, id);
    if (expired_overrun != 0) {
        errno = EPROTO;
        return fail("timer_getoverrun-periodic-latched");
    }
    if (syscall(SYS_timer_delete, id) != 0) {
        return fail("timer_getoverrun-periodic-delete");
    }
    ASSERT("timer_getoverrun", "SIGEV_NONE_NEVER_LATCHES_OVERRUN");

    RESULT("timer_getoverrun");
    return 0;
}

/* `do_sys_settimeofday64()` validates the wall-clock bound *before* the
 * CAP_SYS_TIME test and the timezone range *after* it
 * (`kernel/time/time.c:174-197`), an ordering that is only observable without
 * privilege.  This case therefore runs last and leaves the process
 * unprivileged; nothing after it depends on credentials. */
static int case_settimeofday_unprivileged(void) {
    struct timeval tv;
    struct timezone tz;

    CASE("settimeofday-unprivileged");

    memset(&tv, 0, sizeof(tv));
    memset(&tz, 0, sizeof(tz));
    if (setgid(65534) != 0 || setuid(65534) != 0) {
        return fail("settimeofday-unprivileged-drop");
    }

    /* An oversized wall time is rejected by the bound before the capability
     * test, so even an unprivileged caller sees EINVAL. */
    tv.tv_sec = ABI_TIME_SETTOD_SEC_MAX;
    errno = 0;
    if (raw_settimeofday(&tv, &tz) != -1 || errno != EINVAL) {
        return fail("settimeofday-unprivileged-bound");
    }
    ASSERT("settimeofday-unprivileged", "BOUND_BEFORE_CAPABILITY");

    /* A well-formed wall time reaches the capability test. */
    tv.tv_sec = 1000000000;
    errno = 0;
    if (raw_settimeofday(&tv, &tz) != -1 || errno != EPERM) {
        return fail("settimeofday-unprivileged-capability");
    }
    /* The timezone range is validated after the capability test, so a range
     * violation is still EPERM rather than EINVAL. */
    tz.tz_minuteswest = 15 * 60 + 1;
    errno = 0;
    if (raw_settimeofday(&tv, &tz) != -1 || errno != EPERM) {
        return fail("settimeofday-unprivileged-timezone");
    }
    ASSERT("settimeofday-unprivileged", "CAPABILITY_BEFORE_TIMEZONE_RANGE");

    /* The alarm-sleep admission order is only observable without privilege: a
     * kernel with an RTC must report EINVAL for a flag bit other than
     * TIMER_ABSTIME before it reports EPERM for the missing CAP_WAKE_ALARM
     * (`alarm_timer_nsleep()`, kernel/time/alarmtimer.c:775-782).  Without an
     * RTC the RTC test wins and both calls are EOPNOTSUPP, which is the
     * oracle's configuration and is asserted as such. */
    const struct timespec zero_delay = {0, 0};
    errno = 0;
    long alarm_bad_flags = raw_clock_nanosleep(CLOCK_REALTIME_ALARM, 0x8000, &zero_delay);
    int alarm_bad_errno = errno;
    errno = 0;
    long alarm_plain = raw_clock_nanosleep(CLOCK_REALTIME_ALARM, 0, &zero_delay);
    int alarm_plain_errno = errno;
    if (alarm_bad_flags == 0) {
        errno = EPROTO;
        return fail("clock_nanosleep-unprivileged-alarm-armed");
    }
    if (alarm_bad_errno == EOPNOTSUPP) {
        if (alarm_plain != -1 || alarm_plain_errno != EOPNOTSUPP) {
            errno = EPROTO;
            return fail("clock_nanosleep-unprivileged-alarm-no-rtc");
        }
    } else if (alarm_bad_errno != EINVAL || (alarm_plain != -1 || alarm_plain_errno != EPERM)) {
        errno = EPROTO;
        return fail("clock_nanosleep-unprivileged-alarm-order");
    }
    ASSERT("settimeofday-unprivileged", "ALARM_FLAGS_BEFORE_CAPABILITY");

    RESULT("settimeofday-unprivileged");
    return 0;
}

int main(void) {
    /* A signal handler is required by the SIGEV_THREAD contract, and its
     * presence must not disturb the timer identity assertions. */
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = on_expire;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, NULL) != 0) {
        return fail("sigaction");
    }
    if (case_gettimeofday() || case_settimeofday() || case_clock_settime() ||
        case_clock_getres() || case_clock_nanosleep() || case_adjtimex() ||
        case_clock_adjtime() || case_timer_create() ||
        case_timer_getoverrun() || case_settimeofday_unprivileged()) {
        return 1;
    }
    puts("THEKERNEL_TIME_ABI_OK");
    return 0;
}
