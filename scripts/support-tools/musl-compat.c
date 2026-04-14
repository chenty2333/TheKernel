#define _GNU_SOURCE

#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <sched.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>

typedef int (*clone_fn_t)(int (*)(void *), void *, int, void *, pid_t *, void *, pid_t *);
typedef int (*nice_fn_t)(int);
typedef long (*pathconf_fn_t)(const char *, int);
typedef char *(*realpath_fn_t)(const char *, char *);

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

static pathconf_fn_t resolve_real_pathconf(void)
{
    static pathconf_fn_t real_pathconf;
    static bool resolved;

    if (!resolved) {
        resolved = true;
        real_pathconf = (pathconf_fn_t)dlsym(RTLD_NEXT, "pathconf");
    }
    return real_pathconf;
}

static realpath_fn_t resolve_real_realpath(void)
{
    static realpath_fn_t real_realpath;
    static bool resolved;

    if (!resolved) {
        resolved = true;
        real_realpath = (realpath_fn_t)dlsym(RTLD_NEXT, "realpath");
    }
    return real_realpath;
}

static int validate_pathconf_path(const char *path)
{
    int fd;

    if (!path) {
        errno = EFAULT;
        return -1;
    }
    if (path[0] == '\0') {
        errno = ENOENT;
        return -1;
    }
    if (strlen(path) > PATH_MAX) {
        errno = ENAMETOOLONG;
        return -1;
    }

    fd = open(path, O_RDONLY | O_CLOEXEC | O_NONBLOCK);
    if (fd < 0) {
        return -1;
    }
    close(fd);
    return 0;
}

static int join_symlink_target(const char *path, const char *target, char *out, size_t out_size)
{
    const char *slash;
    size_t prefix_len;
    size_t target_len = strlen(target);

    if (target[0] == '/') {
        if (target_len + 1 > out_size) {
            errno = ENAMETOOLONG;
            return -1;
        }
        memcpy(out, target, target_len + 1);
        return 0;
    }

    slash = strrchr(path, '/');
    prefix_len = slash ? (size_t)(slash - path + 1) : 0;
    if (prefix_len + target_len + 1 > out_size) {
        errno = ENAMETOOLONG;
        return -1;
    }

    if (prefix_len > 0) {
        memcpy(out, path, prefix_len);
    }
    memcpy(out + prefix_len, target, target_len + 1);
    return 0;
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

long pathconf(const char *path, int name)
{
    pathconf_fn_t real_pathconf;

    if (name < 0) {
        errno = EINVAL;
        return -1;
    }
    if (validate_pathconf_path(path) < 0) {
        return -1;
    }

    real_pathconf = resolve_real_pathconf();
    if (!real_pathconf) {
        errno = ENOSYS;
        return -1;
    }

    return real_pathconf(path, name);
}

char *realpath(const char *path, char *resolved_path)
{
    realpath_fn_t real_realpath = resolve_real_realpath();
    char target[PATH_MAX + 1];
    char candidate[PATH_MAX + 1];
    ssize_t target_len;

    if (real_realpath) {
        char *resolved = real_realpath(path, resolved_path);
        if (resolved || errno != ELOOP) {
            return resolved;
        }
    }

    if (!path) {
        errno = EFAULT;
        return NULL;
    }
    if (path[0] == '\0') {
        errno = ENOENT;
        return NULL;
    }
    if (strlen(path) > PATH_MAX) {
        errno = ENAMETOOLONG;
        return NULL;
    }

    target_len = readlink(path, target, PATH_MAX);
    if (target_len < 0) {
        return NULL;
    }
    target[target_len] = '\0';
    if (join_symlink_target(path, target, candidate, sizeof(candidate)) < 0) {
        return NULL;
    }

    if (real_realpath) {
        return real_realpath(candidate, resolved_path);
    }

    if (resolved_path) {
        size_t len = strlen(candidate) + 1;
        memcpy(resolved_path, candidate, len);
        return resolved_path;
    }

    return strdup(candidate);
}

int gethostname(char *name, size_t len)
{
    struct utsname uts;
    size_t host_len;

    if (uname(&uts) < 0) {
        return -1;
    }

    host_len = strnlen(uts.nodename, sizeof(uts.nodename));
    if (len <= host_len) {
        errno = ENAMETOOLONG;
        return -1;
    }

    memcpy(name, uts.nodename, host_len);
    name[host_len] = '\0';
    return 0;
}
