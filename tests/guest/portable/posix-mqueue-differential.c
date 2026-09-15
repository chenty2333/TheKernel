/*
 * POSIX message queue ABI differential (syscalls 240..244).
 *
 * Every record printed here is produced by both TheKernel and a real Linux
 * 7.2.3 guest, so the program only asserts behaviour that is identical on the
 * two.  All queue operations use the raw syscalls: the glibc wrappers rewrite
 * `struct sigevent` for SIGEV_THREAD and always pass a valid attribute
 * pointer, which would hide the argument-precedence rules under test.
 *
 * Reference: Linux v7.2.3 `ipc/mqueue.c` (`do_mq_open`, `prepare_open`,
 * `do_mq_timedsend`, `do_mq_timedreceive`, `do_mq_notify`, `mqueue_get_inode`)
 * and `fs/namei.c` (`may_delete_dentry`, `check_sticky`).
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <mqueue.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Linux `MQ_PRIO_MAX`; glibc does not expose it in <mqueue.h>. */
#define LINUX_MQ_PRIO_MAX 32768
/* `HARD_MSGSIZEMAX` from `ipc/mqueue.c`. */
#define LINUX_HARD_MSGSIZE_MAX (16L * 1024 * 1024)
/* `NOTIFY_COOKIE_LEN` from `ipc/mqueue.c`. */
#define LINUX_NOTIFY_COOKIE_LEN 32
#define MESSAGE_LIMIT 4096

static char queue_names[6][160];

static int fail(const char *stage) {
    fprintf(stderr, "posix-mqueue-differential: %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static int fail_code(const char *stage, long code) {
    fprintf(stderr, "posix-mqueue-differential: %s code=%ld\n", stage, code);
    return 1;
}

static struct mq_attr attr_of(long maxmsg, long msgsize) {
    struct mq_attr attr;
    memset(&attr, 0, sizeof(attr));
    attr.mq_maxmsg = maxmsg;
    attr.mq_msgsize = msgsize;
    return attr;
}

static int raw_mq_open(const char *name, int oflag, mode_t mode,
                       const struct mq_attr *attr) {
    return (int)syscall(SYS_mq_open, name, oflag, (unsigned int)mode, attr);
}

static int raw_mq_unlink(const char *name) {
    return (int)syscall(SYS_mq_unlink, name);
}

static long raw_mq_timedsend(int fd, const char *msg, size_t len,
                             unsigned int prio, const struct timespec *ts) {
    return syscall(SYS_mq_timedsend, fd, msg, len, prio, ts);
}

static long raw_mq_timedreceive(int fd, char *msg, size_t len,
                                unsigned int *prio,
                                const struct timespec *ts) {
    return syscall(SYS_mq_timedreceive, fd, msg, len, prio, ts);
}

static int raw_mq_notify(int fd, const struct sigevent *event) {
    return (int)syscall(SYS_mq_notify, fd, event);
}

/* `mq_getsetattr(fd, new, old)`: a NULL new attribute reads the queue. */
static int raw_mq_getattr(int fd, struct mq_attr *out) {
    return (int)syscall(SYS_mq_getsetattr, fd, NULL, out);
}

static int raw_mq_setattr(int fd, const struct mq_attr *in,
                          struct mq_attr *out) {
    return (int)syscall(SYS_mq_getsetattr, fd, in, out);
}

static volatile sig_atomic_t notify_count;
static volatile sig_atomic_t notify_signo;
static volatile sig_atomic_t notify_code;
static volatile sig_atomic_t notify_value;

static void notify_handler(int signo, siginfo_t *info, void *context) {
    (void)context;
    notify_count++;
    notify_signo = signo;
    if (info != NULL) {
        notify_code = info->si_code;
        notify_value = info->si_value.sival_int;
    }
}

static int install_notify_handler(void) {
    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_sigaction = notify_handler;
    action.sa_flags = SA_SIGINFO | SA_RESTART;
    sigemptyset(&action.sa_mask);
    return sigaction(SIGUSR1, &action, NULL);
}

/* Waits up to `limit_ms` for the counter to reach `want`, so delivery latency
 * never decides the outcome while a missing signal still fails deterministically. */
static int await_notifications(sig_atomic_t want, int limit_ms) {
    int waited = 0;
    while (notify_count < want && waited < limit_ms) {
        struct timespec pause = {0, 10 * 1000 * 1000};
        nanosleep(&pause, NULL);
        waited += 10;
    }
    return notify_count == want;
}

static void deadline_in(struct timespec *ts, long seconds) {
    clock_gettime(CLOCK_REALTIME, ts);
    ts->tv_sec += seconds;
}

static void deadline_expired(struct timespec *ts) {
    ts->tv_sec = 0;
    ts->tv_nsec = 0;
}

static void message_text(char *buffer, size_t size, const char *tag) {
    memset(buffer, 0, size);
    (void)snprintf(buffer, size, "%s", tag);
}

/*
 * `mqueue_get_inode()` admission and charging.
 *
 * The charge is `mq_maxmsg * mq_msgsize + mq_maxmsg * sizeof(struct msg_msg)
 * + min(mq_maxmsg, MQ_PRIO_MAX) * sizeof(struct posix_msg_tree_node)`, i.e.
 * 4096 + 48 + 48 = 4192 bytes for {1, 4096}, and it is compared against the
 * caller's `RLIMIT_MSGQUEUE` without a capability bypass.
 */
static int case_mq_open(void) {
    struct mq_attr attr, observed;
    struct rlimit original, limit;
    char payload[MESSAGE_LIMIT];
    char long_name[300];
    int fd, second, third;
    long i;

    /* A NULL attribute means min(mq_msg_max, DFLT_MSGMAX) / min(mq_msgsize_max,
     * DFLT_MSGSIZEMAX) from `mq_init_ns()`: 10 messages of 8192 bytes. */
    fd = raw_mq_open(queue_names[0], O_CREAT | O_EXCL | O_RDWR, 0600, NULL);
    if (fd < 0) {
        return fail("default-open");
    }
    if (raw_mq_getattr(fd, &observed) != 0) {
        return fail("default-getattr");
    }
    if (observed.mq_maxmsg != 10 || observed.mq_msgsize != 8192 ||
        observed.mq_curmsgs != 0 || observed.mq_flags != 0) {
        return fail_code("default-attributes", observed.mq_maxmsg);
    }
    /* `do_mq_open()` installs the descriptor with `FD_ADD(O_CLOEXEC, ...)`
     * (`ipc/mqueue.c:924`), and `FD_ADD` hands that argument straight to
     * `get_unused_fd_flags()`. The queue descriptor is close-on-exec whether or
     * not the caller passed `O_CLOEXEC`; `oflag` only reaches
     * `dentry_open()`'s `f_flags`. */
    int descriptor_flags = fcntl(fd, F_GETFD);
    if (descriptor_flags < 0 || (descriptor_flags & FD_CLOEXEC) == 0) {
        return fail_code("open-cloexec-unconditional", (long)descriptor_flags);
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential FD_CLOEXEC_UNCONDITIONAL pass");
    if (close(fd) != 0) {
        return fail("default-close");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential DEFAULT_ATTRIBUTES pass");

    /* `CAP_SYS_RESOURCE` replaces the namespace maxima with HARD_MSGMAX and
     * HARD_MSGSIZEMAX, so a privileged caller may exceed /proc defaults but
     * never the hard limits. */
    attr = attr_of(100, 64);
    fd = raw_mq_open(queue_names[1], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (fd < 0) {
        return fail("cap-sys-resource-open");
    }
    if (raw_mq_getattr(fd, &observed) != 0 || observed.mq_maxmsg != 100 ||
        observed.mq_msgsize != 64) {
        return fail("cap-sys-resource-attributes");
    }
    if (close(fd) != 0) {
        return fail("cap-sys-resource-close");
    }
    attr = attr_of(65537, 64);
    errno = 0;
    if (raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr) != -1 ||
        errno != EINVAL) {
        return fail("hard-msgmax");
    }
    attr = attr_of(1, LINUX_HARD_MSGSIZE_MAX + 1);
    errno = 0;
    if (raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr) != -1 ||
        errno != EINVAL) {
        return fail("hard-msgsizemax");
    }
    attr = attr_of(0, 64);
    errno = 0;
    if (raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr) != -1 ||
        errno != EINVAL) {
        return fail("zero-maxmsg");
    }
    attr = attr_of(1, 0);
    errno = 0;
    if (raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr) != -1 ||
        errno != EINVAL) {
        return fail("zero-msgsize");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential HARD_LIMIT_CAPABILITY pass");

    /* RLIMIT_MSGQUEUE accounting runs in a child that switches to an unused
     * uid, so the per-user charge baseline is exactly zero on both guests. */
    if (getrlimit(RLIMIT_MSGQUEUE, &original) != 0) {
        return fail("rlimit-read");
    }
    pid_t child = fork();
    if (child < 0) {
        return fail("rlimit-fork");
    }
    if (child == 0) {
        int code = 0;
        if (setresuid(1234, 1234, 1234) != 0) {
            _exit(120);
        }
        limit.rlim_cur = 4191;
        limit.rlim_max = original.rlim_max;
        if (setrlimit(RLIMIT_MSGQUEUE, &limit) != 0) {
            _exit(121);
        }
        attr = attr_of(1, 4096);
        errno = 0;
        /* 4192 accounted bytes over a 4191 byte limit: EMFILE, not EAGAIN. */
        if (raw_mq_open(queue_names[3], O_CREAT | O_EXCL | O_RDWR, 0600, &attr) != -1 ||
            errno != EMFILE) {
            code = 122;
        }
        limit.rlim_cur = 4192;
        if (code == 0 && setrlimit(RLIMIT_MSGQUEUE, &limit) != 0) {
            code = 123;
        }
        if (code == 0) {
            fd = raw_mq_open(queue_names[3], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
            if (fd < 0) {
                code = 124;
            } else {
                /* unlink + last close evicts the inode and releases the
                 * charge, so the same attributes fit again. */
                if (raw_mq_unlink(queue_names[3]) != 0 || close(fd) != 0) {
                    code = 125;
                } else {
                    struct timespec settle = {0, 100 * 1000 * 1000};
                    nanosleep(&settle, NULL);
                    fd = raw_mq_open(queue_names[3], O_CREAT | O_EXCL | O_RDWR, 0600,
                                     &attr);
                    if (fd < 0) {
                        code = 126;
                    } else {
                        if (raw_mq_unlink(queue_names[3]) != 0 || close(fd) != 0) {
                            code = 127;
                        }
                    }
                }
            }
        }
        _exit(code);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail("rlimit-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("rlimit-charge-boundary",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential RLIMIT_CHARGE_BOUNDARY pass");

    /* `prepare_open()` copies the attribute before the name is resolved, and
     * for an existing queue checks EEXIST, the reserved O_RDWR|O_WRONLY access
     * mode, and only then the inode permission. */
    attr = attr_of(10, 8192);
    fd = raw_mq_open(queue_names[4], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (fd < 0) {
        return fail("existing-open");
    }
    errno = 0;
    if (raw_mq_open(queue_names[4], O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 ||
        errno != EEXIST) {
        return fail("existing-exclusive");
    }
    errno = 0;
    if (raw_mq_open(queue_names[4], O_CREAT | O_EXCL | O_RDWR, 0600,
                    (const struct mq_attr *)1) != -1 ||
        errno != EFAULT) {
        return fail("existing-attr-fault");
    }
    errno = 0;
    if (raw_mq_open(queue_names[4], O_CREAT | O_RDWR | O_WRONLY, 0600, NULL) != -1 ||
        errno != EINVAL) {
        return fail("existing-reserved-accmode");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential OPEN_EXISTING_PRECEDENCE pass");

    /* A new name accepts the reserved access mode and yields a descriptor with
     * neither FMODE_READ nor FMODE_WRITE: OPEN_FMODE((O_RDWR|O_WRONLY) + 1)
     * clears both bits. */
    attr = attr_of(4, 64);
    third = raw_mq_open(queue_names[5], O_CREAT | O_EXCL | O_RDWR | O_WRONLY, 0600,
                        &attr);
    if (third < 0) {
        return fail("accmode-three-open");
    }
    message_text(payload, sizeof(payload), "x");
    errno = 0;
    if (raw_mq_timedsend(third, payload, 1, 0, NULL) != -1 || errno != EBADF) {
        return fail("accmode-three-send");
    }
    errno = 0;
    if (raw_mq_timedreceive(third, payload, sizeof(payload), NULL, NULL) != -1 ||
        errno != EBADF) {
        return fail("accmode-three-receive");
    }
    if (close(third) != 0) {
        return fail("accmode-three-close");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential CREATE_ACCMODE_THREE pass");

    /* `do_getname()` maps the empty name to ENOENT before any lookup. */
    errno = 0;
    if (raw_mq_open("", O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 || errno != ENOENT) {
        return fail("name-empty");
    }
    /* `name_is_dot_dotdot()` covers exactly "." and ".."; both are EACCES. */
    errno = 0;
    if (raw_mq_open(".", O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 || errno != EACCES) {
        return fail("name-dot");
    }
    errno = 0;
    if (raw_mq_open("..", O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 || errno != EACCES) {
        return fail("name-dotdot");
    }
    /* The raw syscall takes the slash-free kernel name and rejects every '/'. */
    errno = 0;
    if (raw_mq_open("/tk-mq-diff-slash", O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 ||
        errno != EACCES) {
        return fail("name-leading-slash");
    }
    errno = 0;
    if (raw_mq_open("tk-mq-diff-a/b", O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 ||
        errno != EACCES) {
        return fail("name-embedded-slash");
    }
    second = raw_mq_open("...", O_CREAT | O_EXCL | O_RDWR, 0600, NULL);
    if (second < 0) {
        return fail("name-three-dots");
    }
    if (raw_mq_unlink("...") != 0 || close(second) != 0) {
        return fail("name-three-dots-teardown");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential NAME_SYNTAX pass");

    /* NAME_MAX is inclusive; `simple_lookup()` reports ENAMETOOLONG above. */
    for (i = 0; i < 255; i++) {
        long_name[i] = (char)('a' + (i % 26));
    }
    long_name[255] = '\0';
    second = raw_mq_open(long_name, O_CREAT | O_EXCL | O_RDWR, 0600, NULL);
    if (second < 0) {
        return fail("name-max-boundary");
    }
    if (raw_mq_unlink(long_name) != 0 || close(second) != 0) {
        return fail("name-max-teardown");
    }
    for (i = 0; i < 256; i++) {
        long_name[i] = (char)('a' + (i % 26));
    }
    long_name[256] = '\0';
    errno = 0;
    if (raw_mq_open(long_name, O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 ||
        errno != ENAMETOOLONG) {
        return fail("name-too-long");
    }
    /* The syntax checks run before the length check: an over-long name that
     * also holds a slash reports EACCES, not ENAMETOOLONG. */
    long_name[0] = '/';
    errno = 0;
    if (raw_mq_open(long_name, O_CREAT | O_EXCL | O_RDWR, 0600, NULL) != -1 ||
        errno != EACCES) {
        return fail("name-slash-before-length");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential NAME_LENGTH_BOUNDARY pass");

    if (raw_mq_unlink(queue_names[4]) != 0) {
        return fail("existing-unlink");
    }
    if (close(fd) != 0) {
        return fail("existing-close");
    }
    if (raw_mq_unlink(queue_names[5]) != 0) {
        return fail("accmode-three-unlink");
    }
    /* queue_names[0] must survive for the unlink authority case. */
    if (raw_mq_unlink(queue_names[1]) != 0) {
        return fail("cap-sys-resource-unlink");
    }
    puts("THEKERNEL_ABI_ASSERT mq_open.raw-differential OPEN_FD_LIFETIME pass");
    return 0;
}

/*
 * `may_delete_dentry()` order: the sticky root directory is world writable, so
 * a non-owner fails `check_sticky()` with EPERM.  Queue owner, directory owner
 * and CAP_FOWNER all pass.
 */
static int case_mq_unlink(void) {
    char payload[64];
    int fd;

    if (geteuid() != 0) {
        return fail_code("unlink-requires-root", (long)geteuid());
    }

    /* `SYSCALL_DEFINE1(mq_unlink)` shares `do_getname()` and
     * `lookup_noperm_common()` with `mq_open()`, so the name rules match. */
    errno = 0;
    if (raw_mq_unlink("") != -1 || errno != ENOENT) {
        return fail("unlink-empty-name");
    }
    errno = 0;
    if (raw_mq_unlink("/tk-mq-diff-slash") != -1 || errno != EACCES) {
        return fail("unlink-slash-name");
    }
    errno = 0;
    if (raw_mq_unlink(".") != -1 || errno != EACCES) {
        return fail("unlink-dot-name");
    }
    errno = 0;
    if (raw_mq_unlink("tk-mq-diff-missing") != -1 || errno != ENOENT) {
        return fail("unlink-missing-name");
    }
    puts("THEKERNEL_ABI_ASSERT mq_unlink.raw-differential UNLINK_NAME_SYNTAX pass");

    pid_t child = fork();
    if (child < 0) {
        return fail("unlink-foreign-fork");
    }
    if (child == 0) {
        if (setresuid(1001, 1001, 1001) != 0) {
            _exit(120);
        }
        errno = 0;
        int rc = raw_mq_unlink(queue_names[0]);
        if (rc != -1 || errno != EPERM) {
            _exit(errno == EACCES ? 2 : 1);
        }
        _exit(0);
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail("unlink-foreign-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("unlink-foreign-sticky",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    puts("THEKERNEL_ABI_ASSERT mq_unlink.raw-differential STICKY_DIRECTORY_EPERM pass");

    /* The refused unlink left the name resolvable. */
    fd = raw_mq_open(queue_names[0], O_RDONLY | O_NONBLOCK, 0, NULL);
    if (fd < 0) {
        return fail("unlink-survivor-open");
    }
    if (close(fd) != 0) {
        return fail("unlink-survivor-close");
    }

    /* An unlinked queue stays usable through retained descriptors, and the
     * name is immediately reusable. */
    if (raw_mq_unlink(queue_names[0]) != 0) {
        return fail("unlink-owner");
    }
    fd = raw_mq_open(queue_names[0], O_CREAT | O_EXCL | O_RDWR | O_NONBLOCK, 0600,
                     NULL);
    if (fd < 0) {
        return fail("unlink-reuse-open");
    }
    if (raw_mq_unlink(queue_names[0]) != 0) {
        return fail("unlink-reuse-unlink");
    }
    message_text(payload, sizeof(payload), "retained");
    if (raw_mq_timedsend(fd, payload, strlen(payload), 0, NULL) != 0) {
        return fail("unlink-retained-send");
    }
    if (close(fd) != 0) {
        return fail("unlink-retained-close");
    }

    errno = 0;
    if (raw_mq_unlink(queue_names[0]) != -1 || errno != ENOENT) {
        return fail("unlink-missing");
    }
    puts("THEKERNEL_ABI_ASSERT mq_unlink.raw-differential OWNER_AND_REUSE pass");

    child = fork();
    if (child < 0) {
        return fail("unlink-owner-create-fork");
    }
    if (child == 0) {
        if (setresuid(1000, 1000, 1000) != 0) {
            _exit(120);
        }
        int owned = raw_mq_open(queue_names[1], O_CREAT | O_EXCL | O_RDWR, 0600, NULL);
        if (owned < 0) {
            _exit(1);
        }
        if (close(owned) != 0) {
            _exit(2);
        }
        _exit(0);
    }
    if (waitpid(child, &status, 0) != child) {
        return fail("unlink-owner-create-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("unlink-owner-create",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    /* The queue owner passes `check_sticky()` even when the directory is owned
     * by another uid, and the initial user namespace root unlinks any queue. */
    child = fork();
    if (child < 0) {
        return fail("unlink-owner-fork");
    }
    if (child == 0) {
        if (setresuid(1000, 1000, 1000) != 0) {
            _exit(120);
        }
        _exit(raw_mq_unlink(queue_names[1]) == 0 ? 0 : 1);
    }
    if (waitpid(child, &status, 0) != child) {
        return fail("unlink-owner-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("unlink-directory-owner",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    child = fork();
    if (child < 0) {
        return fail("unlink-cap-fork");
    }
    if (child == 0) {
        if (setresuid(1000, 1000, 1000) != 0) {
            _exit(120);
        }
        int owned = raw_mq_open(queue_names[1], O_CREAT | O_EXCL | O_RDWR, 0600, NULL);
        if (owned < 0) {
            _exit(1);
        }
        _exit(close(owned) == 0 ? 0 : 2);
    }
    if (waitpid(child, &status, 0) != child) {
        return fail("unlink-cap-wait");
    }
    if (raw_mq_unlink(queue_names[1]) != 0) {
        return fail("unlink-cap-fowner");
    }
    puts("THEKERNEL_ABI_ASSERT mq_unlink.raw-differential OWNER_AUTHORITY pass");
    return 0;
}

/*
 * `do_mq_timedsend()`: the syscall wrapper prepares the absolute timeout
 * before the priority check, the descriptor and mode are validated next, and
 * `-EAGAIN` is the only answer for a full queue opened O_NONBLOCK.
 */
static int case_mq_timedsend(void) {
    struct mq_attr attr;
    struct timespec past, future;
    char payload[64];
    char received[64];
    unsigned int priority = 0;
    int fd, small;
    long index;

    attr = attr_of(2, 32);
    fd = raw_mq_open(queue_names[0], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (fd < 0) {
        return fail("send-open");
    }

    message_text(payload, sizeof(payload), "m");
    deadline_expired(&past);

    /* Bad timeout pointer outranks an invalid priority. */
    errno = 0;
    if (raw_mq_timedsend(fd, payload, 1, LINUX_MQ_PRIO_MAX, &past) != -1 ||
        errno != EINVAL) {
        return fail("send-priority-before-timeout");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, payload, 1, LINUX_MQ_PRIO_MAX,
                         (const struct timespec *)1) != -1 ||
        errno != EFAULT) {
        return fail("send-timeout-first");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, payload, 1, LINUX_MQ_PRIO_MAX, NULL) != -1 ||
        errno != EINVAL) {
        return fail("send-priority-invalid");
    }
    errno = 0;
    if (raw_mq_timedsend(-1, payload, 1, 0, NULL) != -1 || errno != EBADF) {
        return fail("send-bad-descriptor");
    }
    /* The read-only descriptor fails before the message size is considered. */
    small = raw_mq_open(queue_names[0], O_RDONLY | O_NONBLOCK, 0, NULL);
    if (small < 0) {
        return fail("send-readonly-open");
    }
    errno = 0;
    if (raw_mq_timedsend(small, payload, 33, 0, NULL) != -1 || errno != EBADF) {
        return fail("send-readonly-descriptor");
    }
    if (close(small) != 0) {
        return fail("send-readonly-close");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, payload, 33, 0, NULL) != -1 || errno != EMSGSIZE) {
        return fail("send-message-too-large");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, (const char *)1, 1, 0, NULL) != -1 ||
        errno != EFAULT) {
        return fail("send-message-fault");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedsend.raw-differential ARGUMENT_PRECEDENCE pass");

    /* Fill the queue, then check the two blocking answers. */
    if (raw_mq_timedsend(fd, "a", 1, 1, NULL) != 0 ||
        raw_mq_timedsend(fd, "b", 1, 2, NULL) != 0) {
        return fail("send-fill");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, "c", 1, 3, &past) != -1 || errno != ETIMEDOUT) {
        return fail("send-full-timeout");
    }
    if (raw_mq_setattr(fd, &(struct mq_attr){.mq_flags = O_NONBLOCK}, NULL) != 0) {
        return fail("send-nonblocking");
    }
    errno = 0;
    if (raw_mq_timedsend(fd, "c", 1, 3, NULL) != -1 || errno != EAGAIN) {
        return fail("send-full-eagain");
    }
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("send-drain-high");
    }
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("send-drain-low");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedsend.raw-differential FULL_QUEUE_EAGAIN pass");

    /* Higher priorities are dequeued first and equal priorities keep their
     * arrival order (msg_insert appends to the leaf's tail). */
    attr = attr_of(8, 32);
    small = raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (small < 0) {
        return fail("send-order-open");
    }
    for (index = 0; index < 4; index++) {
        static const unsigned int priorities[4] = {1, 5, 5, 3};
        static const char tags[4][2] = {"w", "x", "y", "z"};
        if (raw_mq_timedsend(small, tags[index], 1, priorities[index], NULL) != 0) {
            return fail("send-order-insert");
        }
    }
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'x' || priority != 5) {
        return fail("send-order-five-first");
    }
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'y' || priority != 5) {
        return fail("send-order-five-second");
    }
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'z' || priority != 3) {
        return fail("send-order-three");
    }
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'w' || priority != 1) {
        return fail("send-order-one");
    }
    if (close(small) != 0 || raw_mq_unlink(queue_names[2]) != 0) {
        return fail("send-order-teardown");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedsend.raw-differential PRIORITY_ORDER pass");

    /* A blocked sender is handed the slot freed by a receiver
     * (`pipelined_receive`), so the send completes with its own message
     * delivered instead of failing or being re-queued twice. */
    attr = attr_of(1, 32);
    small = raw_mq_open(queue_names[2], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (small < 0) {
        return fail("send-handoff-open");
    }
    if (raw_mq_timedsend(small, "A", 1, 9, NULL) != 0) {
        return fail("send-handoff-fill");
    }
    int ready[2];
    if (pipe(ready) != 0) {
        return fail("send-handoff-pipe");
    }
    pid_t child = fork();
    if (child < 0) {
        return fail("send-handoff-fork");
    }
    if (child == 0) {
        if (close(ready[0]) != 0) {
            _exit(120);
        }
        deadline_in(&future, 10);
        if (write(ready[1], "r", 1) != 1) {
            _exit(121);
        }
        if (raw_mq_timedsend(small, "B", 1, 4, &future) != 0) {
            _exit(122);
        }
        _exit(0);
    }
    if (close(ready[1]) != 0) {
        return fail("send-handoff-close");
    }
    char token = 0;
    if (read(ready[0], &token, 1) != 1) {
        return fail("send-handoff-ready");
    }
    /* The sender is parked in wq_sleep() by now; the receive below must hand
     * the freed slot to it rather than wake it to race for the capacity. */
    struct timespec pause = {0, 500 * 1000 * 1000};
    nanosleep(&pause, NULL);
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'A' || priority != 9) {
        return fail("send-handoff-receive");
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail("send-handoff-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("send-handoff-child",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    if (raw_mq_timedreceive(small, received, sizeof(received), &priority, NULL) != 1 ||
        received[0] != 'B' || priority != 4) {
        return fail("send-handoff-followup");
    }
    if (close(ready[0]) != 0) {
        return fail("send-handoff-pipe-close");
    }
    if (close(small) != 0 || raw_mq_unlink(queue_names[2]) != 0) {
        return fail("send-handoff-teardown");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedsend.raw-differential BLOCKED_SENDER_HANDOFF pass");

    if (close(fd) != 0 || raw_mq_unlink(queue_names[0]) != 0) {
        return fail("send-teardown");
    }
    return 0;
}

/*
 * `do_mq_timedreceive()` checks `msg_len < mq_msgsize` before the empty-queue
 * `-EAGAIN`, dequeues before copying out, and reports the stored priority.
 */
static int case_mq_timedreceive(void) {
    struct mq_attr attr;
    struct timespec past;
    char buffer[64];
    unsigned int priorities[3] = {1, 2, 3};
    unsigned int priority = 0;
    int fd;
    long index;

    attr = attr_of(4, 24);
    fd = raw_mq_open(queue_names[0], O_CREAT | O_EXCL | O_RDWR | O_NONBLOCK, 0600,
                     &attr);
    if (fd < 0) {
        return fail("receive-open");
    }

    /* A buffer below mq_msgsize is EMSGSIZE even though the queue is empty and
     * the descriptor is non-blocking. */
    errno = 0;
    if (raw_mq_timedreceive(fd, buffer, 23, &priority, NULL) != -1 ||
        errno != EMSGSIZE) {
        return fail("receive-emsgsize-first");
    }
    errno = 0;
    if (raw_mq_timedreceive(fd, buffer, sizeof(buffer), &priority, NULL) != -1 ||
        errno != EAGAIN) {
        return fail("receive-empty-eagain");
    }
    errno = 0;
    if (raw_mq_timedreceive(-1, buffer, sizeof(buffer), &priority, NULL) != -1 ||
        errno != EBADF) {
        return fail("receive-bad-descriptor");
    }
    /* A write-only descriptor is rejected after the descriptor lookup and
     * before the size check. */
    int writer = raw_mq_open(queue_names[0], O_WRONLY | O_NONBLOCK, 0, NULL);
    if (writer < 0) {
        return fail("receive-writer-open");
    }
    errno = 0;
    if (raw_mq_timedreceive(writer, buffer, 1, &priority, NULL) != -1 ||
        errno != EBADF) {
        return fail("receive-writeonly-descriptor");
    }
    if (close(writer) != 0) {
        return fail("receive-writer-close");
    }
    /* An absolute timeout in the past times out immediately. */
    deadline_expired(&past);
    if (raw_mq_setattr(fd, &(struct mq_attr){.mq_flags = 0}, NULL) != 0) {
        return fail("receive-blocking");
    }
    errno = 0;
    if (raw_mq_timedreceive(fd, buffer, sizeof(buffer), &priority, &past) != -1 ||
        errno != ETIMEDOUT) {
        return fail("receive-timeout");
    }
    if (raw_mq_setattr(fd, &(struct mq_attr){.mq_flags = O_NONBLOCK}, NULL) != 0) {
        return fail("receive-nonblocking");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedreceive.raw-differential EMSGSIZE_BEFORE_EAGAIN pass");

    for (index = 0; index < 3; index++) {
        static const char tags[3][2] = {"p", "q", "r"};
        if (raw_mq_timedsend(fd, tags[index], 1, priorities[index], NULL) != 0) {
            return fail("receive-order-insert");
        }
    }
    /* msg_get() takes the rightmost (highest priority) leaf first. */
    for (index = 0; index < 3; index++) {
        static const char tags[3][2] = {"r", "q", "p"};
        priority = 0;
        memset(buffer, 0, sizeof(buffer));
        if (raw_mq_timedreceive(fd, buffer, sizeof(buffer), &priority, NULL) != 1 ||
            buffer[0] != tags[index][0] || priority != (unsigned int)(3 - index)) {
            return fail("receive-order");
        }
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedreceive.raw-differential PRIORITY_ORDER pass");

    /* The dequeue commits before the copyout: a faulting destination loses the
     * message but frees the capacity, exactly like Linux. */
    if (raw_mq_timedsend(fd, "s", 1, 7, NULL) != 0 ||
        raw_mq_timedsend(fd, "t", 1, 6, NULL) != 0) {
        return fail("receive-fault-insert");
    }
    errno = 0;
    if (raw_mq_timedreceive(fd, (char *)1, sizeof(buffer), NULL, NULL) != -1 ||
        errno != EFAULT) {
        return fail("receive-copyout-fault");
    }
    priority = 0;
    if (raw_mq_timedreceive(fd, buffer, sizeof(buffer), &priority, NULL) != 1 ||
        buffer[0] != 't' || priority != 6) {
        return fail("receive-after-fault");
    }
    puts("THEKERNEL_ABI_ASSERT mq_timedreceive.raw-differential DEQUEUE_BEFORE_COPYOUT pass");

    if (close(fd) != 0 || raw_mq_unlink(queue_names[0]) != 0) {
        return fail("receive-teardown");
    }
    return 0;
}

/*
 * `__do_notify()` fires only on the empty-to-nonempty edge of a queue that has
 * a registration and no waiting receiver, consumes the registration, and
 * reports SI_MESGQ with the registered sival_int.
 */
static int case_mq_notify(void) {
    struct mq_attr attr;
    struct sigevent event;
    struct timespec future;
    char payload[64];
    char received[64];
    unsigned int priority = 0;
    int fd;

    if (install_notify_handler() != 0) {
        return fail("notify-handler");
    }
    attr = attr_of(4, 32);
    fd = raw_mq_open(queue_names[0], O_CREAT | O_EXCL | O_RDWR, 0600, &attr);
    if (fd < 0) {
        return fail("notify-open");
    }

    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 4242;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-register");
    }
    /* A second registration while one is active is EBUSY. */
    errno = 0;
    if (raw_mq_notify(fd, &event) != -1 || errno != EBUSY) {
        return fail("notify-busy");
    }
    if (raw_mq_timedsend(fd, "1", 1, 0, NULL) != 0) {
        return fail("notify-first-send");
    }
    if (!await_notifications(1, 2000)) {
        return fail_code("notify-signal", (long)notify_count);
    }
    /* The queue is no longer empty, so a second message must not notify. */
    if (raw_mq_timedsend(fd, "2", 1, 0, NULL) != 0) {
        return fail("notify-second-send");
    }
    /* Unregistering after the one-shot fired is a no-op. */
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-clear");
    }
    if (!await_notifications(1, 200)) {
        return fail_code("notify-extra-signal", (long)notify_count);
    }
    if (notify_signo != SIGUSR1 || notify_code != SI_MESGQ || notify_value != 4242) {
        return fail_code("notify-siginfo", (long)notify_code * 100000L + notify_value);
    }
    puts("THEKERNEL_ABI_ASSERT mq_notify.raw-differential ONE_SHOT_EMPTY_EDGE pass");

    /* Draining the queue re-arms the edge: the next message notifies again. */
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1 ||
        raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("notify-drain");
    }
    event.sigev_value.sival_int = 7;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-reregister");
    }
    if (raw_mq_timedsend(fd, "3", 1, 0, NULL) != 0) {
        return fail("notify-third-send");
    }
    if (!await_notifications(2, 2000)) {
        return fail_code("notify-second-edge", (long)notify_count);
    }
    if (notify_value != 7) {
        return fail_code("notify-second-value", (long)notify_value);
    }
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-final-clear");
    }
    puts("THEKERNEL_ABI_ASSERT mq_notify.raw-differential EMPTY_EDGE_REARM pass");

    /* Argument validation precedes the descriptor lookup. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = 99;
    errno = 0;
    if (raw_mq_notify(-1, &event) != -1 || errno != EINVAL) {
        return fail("notify-invalid-mode");
    }
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("notify-zero-signo-drain");
    }
    /* Linux `valid_signal()` is `sig <= _NSIG`, so `sigev_signo == 0` is a
     * successful SIGEV_SIGNAL registration; `__do_notify()` then consumes the
     * one-shot without sending anything ("do_mq_notify() accepts
     * sigev_signo == 0, why??"). */
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = 0;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-zero-signo");
    }
    if (raw_mq_timedsend(fd, "0", 1, 0, NULL) != 0) {
        return fail("notify-zero-signo-send");
    }
    /* Consumed silently, so another registration is not EBUSY. */
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 6;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-zero-signo-consumed");
    }
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-zero-signo-clear");
    }
    if (!await_notifications(2, 200)) {
        return fail_code("notify-zero-signo-signal", (long)notify_count);
    }
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("notify-zero-signo-final-drain");
    }
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_NONE;
    if (raw_mq_notify(-1, &event) != -1 || errno != EBADF) {
        return fail("notify-bad-descriptor");
    }
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-none");
    }
    if (raw_mq_timedsend(fd, "4", 1, 0, NULL) != 0) {
        return fail("notify-none-send");
    }
    /* SIGEV_NONE consumes the one-shot registration silently; a second
     * registration therefore succeeds rather than reporting EBUSY. */
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 5;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-none-consumed");
    }
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-none-clear");
    }
    if (!await_notifications(2, 200)) {
        return fail_code("notify-none-signal", (long)notify_count);
    }
    if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, NULL) != 1) {
        return fail("notify-none-final-drain");
    }
    /* SIGEV_THREAD copies the 32 byte cookie before resolving the netlink
     * descriptor, so a bad cookie pointer is EFAULT rather than EBADF. */
    memset(&event, 0, sizeof(event));
    event.sigev_notify = SIGEV_THREAD;
    event.sigev_signo = -1;
    event.sigev_value.sival_ptr = (void *)1;
    errno = 0;
    if (raw_mq_notify(fd, &event) != -1 || errno != EFAULT) {
        return fail("notify-thread-cookie-first");
    }
    puts("THEKERNEL_ABI_ASSERT mq_notify.raw-differential ARGUMENT_VALIDATION pass");

    /* `mqueue_flush_file()` (`ipc/mqueue.c:658`) is this queue's `->flush`:
     *
     *	spin_lock(&info->lock);
     *	if (task_tgid(current) == info->notify_owner)
     *		remove_notification(info);
     *
     * `filp_close()` runs it for *every* descriptor of the queue the owning
     * process closes, so closing a second, unrelated descriptor disarms the
     * one-shot registration even though the descriptor the registration named
     * stays open. A surviving registration would answer the re-registration
     * below with EBUSY. */
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 31;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-flush-register");
    }
    errno = 0;
    if (raw_mq_notify(fd, &event) != -1 || errno != EBUSY) {
        return fail("notify-flush-busy");
    }
    int sibling = raw_mq_open(queue_names[0], O_RDONLY | O_NONBLOCK, 0, NULL);
    if (sibling < 0) {
        return fail("notify-flush-open");
    }
    if (close(sibling) != 0) {
        return fail("notify-flush-close");
    }
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-flush-disarmed");
    }
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-flush-clear");
    }
    puts("THEKERNEL_ABI_ASSERT mq_notify.raw-differential DESCRIPTOR_FLUSH_DISARMS pass");

    /* A message consumed by an already blocked receiver is handed over
     * directly (`pipelined_send`), so `__do_notify()` never runs and the
     * one-shot registration survives. */
    int ready[2];
    if (pipe(ready) != 0) {
        return fail("notify-handoff-pipe");
    }
    /* Initialise before the fork: only the parent sends this buffer. */
    message_text(payload, sizeof(payload), "handoff");
    pid_t child = fork();
    if (child < 0) {
        return fail("notify-handoff-fork");
    }
    if (child == 0) {
        if (close(ready[0]) != 0) {
            _exit(120);
        }
        deadline_in(&future, 10);
        if (write(ready[1], "r", 1) != 1) {
            _exit(121);
        }
        message_text(received, sizeof(received), "");
        if (raw_mq_timedreceive(fd, received, sizeof(received), &priority, &future) !=
            (long)strlen(payload)) {
            _exit(122);
        }
        if (strcmp(received, payload) != 0 || priority != 11) {
            _exit(123);
        }
        _exit(0);
    }
    if (close(ready[1]) != 0) {
        return fail("notify-handoff-close");
    }
    char token = 0;
    if (read(ready[0], &token, 1) != 1) {
        return fail("notify-handoff-ready");
    }
    struct timespec pause = {0, 500 * 1000 * 1000};
    nanosleep(&pause, NULL);
    sig_atomic_t before = notify_count;
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 99;
    if (raw_mq_notify(fd, &event) != 0) {
        return fail("notify-handoff-register");
    }
    if (raw_mq_timedsend(fd, payload, strlen(payload), 11, NULL) != 0) {
        return fail("notify-handoff-send");
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return fail("notify-handoff-wait");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return fail_code("notify-handoff-child",
                         WIFEXITED(status) ? WEXITSTATUS(status) : -1);
    }
    if (!await_notifications(before, 300)) {
        return fail_code("notify-handoff-signal", (long)notify_count);
    }
    /* The handoff did not consume the registration: the queue is empty again,
     * so the next message fires the still-armed one-shot notification. */
    if (raw_mq_timedsend(fd, "5", 1, 0, NULL) != 0) {
        return fail("notify-handoff-survivor-send");
    }
    if (!await_notifications(before + 1, 2000)) {
        return fail_code("notify-handoff-survivor", (long)notify_count);
    }
    if (notify_value != 99) {
        return fail_code("notify-handoff-survivor-value", (long)notify_value);
    }
    if (raw_mq_notify(fd, NULL) != 0) {
        return fail("notify-handoff-clear");
    }
    puts("THEKERNEL_ABI_ASSERT mq_notify.raw-differential PIPELINED_RECEIVE_SKIPS_NOTIFY pass");

    if (close(ready[0]) != 0) {
        return fail("notify-handoff-pipe-close");
    }
    if (close(fd) != 0 || raw_mq_unlink(queue_names[0]) != 0) {
        return fail("notify-teardown");
    }
    return 0;
}

static int setup(void) {
    for (size_t index = 0; index < sizeof(queue_names) / sizeof(queue_names[0]);
         index++) {
        /* Kernel names are slash-free: libc's mq_open() passes name + 1, and
         * `lookup_noperm_common()` rejects a name containing '/'. */
        int written = snprintf(queue_names[index], sizeof(queue_names[index]),
                               "tk-mq-diff-%d-%zu", (int)getpid(), index);
        if (written <= 0 || (size_t)written >= sizeof(queue_names[index])) {
            return 1;
        }
        (void)raw_mq_unlink(queue_names[index]);
    }
    return 0;
}

int main(void) {
    if (setup() != 0) {
        return fail("setup");
    }

    puts("THEKERNEL_ABI_CASE mq_open.raw-differential");
    if (case_mq_open() != 0) {
        return 1;
    }
    puts("THEKERNEL_ABI_RESULT mq_open.raw-differential pass");

    puts("THEKERNEL_ABI_CASE mq_unlink.raw-differential");
    if (case_mq_unlink() != 0) {
        return 1;
    }
    puts("THEKERNEL_ABI_RESULT mq_unlink.raw-differential pass");

    puts("THEKERNEL_ABI_CASE mq_timedsend.raw-differential");
    if (case_mq_timedsend() != 0) {
        return 1;
    }
    puts("THEKERNEL_ABI_RESULT mq_timedsend.raw-differential pass");

    puts("THEKERNEL_ABI_CASE mq_timedreceive.raw-differential");
    if (case_mq_timedreceive() != 0) {
        return 1;
    }
    puts("THEKERNEL_ABI_RESULT mq_timedreceive.raw-differential pass");

    puts("THEKERNEL_ABI_CASE mq_notify.raw-differential");
    if (case_mq_notify() != 0) {
        return 1;
    }
    puts("THEKERNEL_ABI_RESULT mq_notify.raw-differential pass");

    puts("THEKERNEL_POSIX_MQUEUE_OK");
    return 0;
}
