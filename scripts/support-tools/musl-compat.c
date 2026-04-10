#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <sched.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

typedef int (*clone_fn_t)(int (*)(void *), void *, int, void *, pid_t *, void *, pid_t *);
typedef int (*nice_fn_t)(int);

static clone_fn_t resolve_real_clone(void)
{
    static clone_fn_t real_clone;
    static bool resolved;

    if (!resolved) {
        resolved = true;
        real_clone = (clone_fn_t)dlsym(RTLD_NEXT, "clone");
    }
    return real_clone;
}

static nice_fn_t resolve_real_nice(void)
{
    static nice_fn_t real_nice;
    static bool resolved;

    if (!resolved) {
        resolved = true;
        real_nice = (nice_fn_t)dlsym(RTLD_NEXT, "nice");
    }
    return real_nice;
}

int clone(int (*fn)(void *), void *stack, int flags, void *arg, ...)
{
    pid_t *ptid = NULL;
    void *tls = NULL;
    pid_t *ctid = NULL;

    if (stack == NULL) {
        errno = EINVAL;
        return -1;
    }

    va_list ap;
    va_start(ap, arg);

    /*
     * This preload shim is enabled only for the musl LTP group. LTP routes
     * clone() through ltp_clone()/ltp_clone7(), which always calls
     * clone(fn, stack, flags, arg, ptid, tls, ctid) and passes NULL
     * placeholders for unused slots. Decode the tail exactly in that order so
     * CHILD_* does not get shifted into ptid on RV musl.
     */
    ptid = va_arg(ap, pid_t *);
    tls = va_arg(ap, void *);
    ctid = va_arg(ap, pid_t *);
    va_end(ap);

    clone_fn_t real_clone = resolve_real_clone();
    if (!real_clone) {
        errno = ENOSYS;
        return -1;
    }

    return real_clone(fn, stack, flags, arg, ptid, tls, ctid);
}

int epoll_create(int size)
{
    if (size <= 0) {
        errno = EINVAL;
        return -1;
    }

    return syscall(SYS_epoll_create1, 0);
}

int nice(int inc)
{
    nice_fn_t real_nice = resolve_real_nice();

    if (!real_nice) {
        errno = ENOSYS;
        return -1;
    }

    int ret = real_nice(inc);
    if (ret == -1 && errno == EACCES) {
        errno = EPERM;
    }
    return ret;
}
