#define _GNU_SOURCE

#include <errno.h>
#include <time.h>
#include <unistd.h>
#include <sys/syscall.h>

int clock_gettime(clockid_t clock_id, struct timespec *tp)
{
    return syscall(SYS_clock_gettime, clock_id, tp);
}

int clock_nanosleep(clockid_t clock_id, int flags,
                    const struct timespec *req, struct timespec *rem)
{
    long ret = syscall(SYS_clock_nanosleep, clock_id, flags, req, rem);
    int err;

    if (ret == 0) {
        return 0;
    }
    err = errno ? errno : EINVAL;
    if (err == EOPNOTSUPP && clock_id == CLOCK_THREAD_CPUTIME_ID) {
        return EINVAL;
    }
    return err;
}
