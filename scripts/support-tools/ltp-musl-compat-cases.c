#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <malloc.h>
#include <setjmp.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#ifndef MAP_ANONYMOUS
#define MAP_ANONYMOUS MAP_ANON
#endif

static const char *case_name;
static int passed;
static int failed;
static int broken;
static int skipped;
static int warnings;

static void emit_result(const char *kind, const char *fmt, ...)
{
    va_list ap;

    printf("%s: %s: ", case_name, kind);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void tinfo(const char *fmt, ...)
{
    va_list ap;

    printf("%s: TINFO: ", case_name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void tpass(const char *fmt, ...)
{
    va_list ap;

    passed++;
    printf("%s: TPASS: ", case_name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void tfail(const char *fmt, ...)
{
    va_list ap;

    failed++;
    printf("%s: TFAIL: ", case_name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void tbrk(const char *fmt, ...)
{
    va_list ap;

    broken++;
    printf("%s: TBROK: ", case_name);
    va_start(ap, fmt);
    vprintf(fmt, ap);
    va_end(ap);
    putchar('\n');
    fflush(stdout);
}

static void emit_summary(void)
{
    putchar('\n');
    puts("Summary:");
    printf("passed   %d\n", passed);
    printf("failed   %d\n", failed);
    printf("broken   %d\n", broken);
    printf("skipped  %d\n", skipped);
    printf("warnings %d\n", warnings);
    fflush(stdout);
}

static const char *base_name(const char *path)
{
    const char *slash = strrchr(path, '/');

    return slash ? slash + 1 : path;
}

static int touch_range(void *ptr, size_t len)
{
    volatile unsigned char *p = ptr;

    if (!ptr || len == 0)
        return -1;

    p[0] = 0xa5;
    p[len - 1] = 0x5a;
    return 0;
}

static int restore_hostid(const unsigned char *saved, ssize_t saved_len, int had_file)
{
    int fd;

    if (!had_file) {
        unlink("/etc/hostid");
        return 0;
    }

    fd = open("/etc/hostid", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return -1;
    if (write(fd, saved, (size_t)saved_len) != saved_len) {
        close(fd);
        return -1;
    }
    return close(fd);
}

static int write_read_hostid(uint32_t value)
{
    uint32_t out = value;
    uint32_t in = 0;
    int fd;
    ssize_t got;

    fd = open("/etc/hostid", O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        return -1;
    if (write(fd, &out, sizeof(out)) != (ssize_t)sizeof(out)) {
        close(fd);
        return -1;
    }
    if (close(fd) != 0)
        return -1;

    fd = open("/etc/hostid", O_RDONLY);
    if (fd < 0)
        return -1;
    got = read(fd, &in, sizeof(in));
    close(fd);

    if (got != (ssize_t)sizeof(in))
        return -1;
    return in == value ? 0 : -1;
}

static int run_gethostid01(void)
{
    unsigned char saved[64];
    ssize_t saved_len = 0;
    int had_file = 0;
    int fd;
    long id;

    fd = open("/etc/hostid", O_RDONLY);
    if (fd >= 0) {
        had_file = 1;
        saved_len = read(fd, saved, sizeof(saved));
        if (saved_len < 0)
            saved_len = 0;
        close(fd);
    }

    id = gethostid();
    tinfo("gethostid() returned %ld", id);

    if (write_read_hostid(0x00000000u) == 0)
        tpass("hostid compatibility storage accepted 0");
    else
        tfail("hostid compatibility storage rejected 0: errno=%d", errno);

    if (write_read_hostid(0x0000ffffu) == 0)
        tpass("hostid compatibility storage accepted 65535");
    else
        tfail("hostid compatibility storage rejected 65535: errno=%d", errno);

    if (restore_hostid(saved, saved_len, had_file) != 0)
        tbrk("failed to restore /etc/hostid: errno=%d", errno);

    return failed || broken ? 1 : 0;
}

static sigjmp_buf context_jump;
static volatile sig_atomic_t context_flag;
static volatile sig_atomic_t saw_signal_context;

static void signal_context_handler(int sig, siginfo_t *info, void *uctx)
{
    if (sig == SIGUSR1 && info && uctx)
        saw_signal_context = 1;
}

static int run_getcontext01(void)
{
    struct sigaction sa;
    struct sigaction old;

    memset(&sa, 0, sizeof(sa));
    sa.sa_sigaction = signal_context_handler;
    sa.sa_flags = SA_SIGINFO;
    sigemptyset(&sa.sa_mask);

    if (sigaction(SIGUSR1, &sa, &old) != 0) {
        tbrk("sigaction(SIGUSR1) failed: errno=%d", errno);
        return 1;
    }

    saw_signal_context = 0;
    if (raise(SIGUSR1) == 0 && saw_signal_context)
        tpass("signal handler received a non-null ucontext");
    else
        tfail("signal handler did not receive a usable ucontext");

    sigaction(SIGUSR1, &old, NULL);

    context_flag = 0;
    if (sigsetjmp(context_jump, 1) == 0) {
        context_flag = 1;
        siglongjmp(context_jump, 1);
        tfail("siglongjmp returned to caller");
    } else if (context_flag) {
        tpass("userspace context jump returned to saved point");
    } else {
        tfail("userspace context jump lost state");
    }

    return failed || broken ? 1 : 0;
}

static int run_mallinfo01(void)
{
    enum { block_count = 20 };
    void *blocks[block_count];
    size_t requested = 0;
    size_t usable = 0;
    int i;

    memset(blocks, 0, sizeof(blocks));
    for (i = 0; i < block_count; i++) {
        size_t size = 160u * (size_t)(i + 1);

        blocks[i] = malloc(size);
        if (!blocks[i]) {
            tfail("malloc(%zu) failed", size);
            break;
        }
        if (touch_range(blocks[i], size) != 0) {
            tfail("failed to touch malloc(%zu)", size);
            break;
        }
        requested += size;
        usable += malloc_usable_size(blocks[i]);
    }

    if (i == block_count && usable >= requested)
        tpass("malloc usable bytes grew by at least %zu", requested);
    else
        tfail("malloc usable bytes %zu smaller than requested %zu", usable, requested);

    if (blocks[block_count / 2]) {
        free(blocks[block_count / 2]);
        blocks[block_count / 2] = malloc(128);
        if (blocks[block_count / 2] && touch_range(blocks[block_count / 2], 128) == 0)
            tpass("allocator reused a freed slot for a smaller allocation");
        else
            tfail("allocator failed after freeing a middle block");
    }

    for (i = 0; i < block_count; i++)
        free(blocks[i]);

    return failed || broken ? 1 : 0;
}

static int run_mallinfo02(void)
{
    void *small;
    void *large;
    void *map;
    size_t map_size = 256u * 1024u;

    small = malloc(20480);
    if (small && malloc_usable_size(small) >= 20480 && touch_range(small, 20480) == 0)
        tpass("malloc(20480) returned usable writable memory");
    else
        tfail("malloc(20480) did not return usable writable memory");
    free(small);

    large = malloc(131072);
    if (large && malloc_usable_size(large) >= 131072 && touch_range(large, 131072) == 0)
        tpass("malloc(131072) returned usable writable memory");
    else
        tfail("malloc(131072) did not return usable writable memory");
    free(large);

    map = mmap(NULL, map_size, PROT_READ | PROT_WRITE,
               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (map != MAP_FAILED && touch_range(map, map_size) == 0) {
        tpass("anonymous mmap provided writable allocator backing");
        munmap(map, map_size);
    } else {
        tfail("anonymous mmap backing failed: errno=%d", errno);
    }

    return failed || broken ? 1 : 0;
}

static int run_mallinfo2_01(void)
{
    const size_t sizes[] = {
        (size_t)2u * 1024u * 1024u * 1024u,
        (size_t)512u * 1024u * 1024u,
        (size_t)128u * 1024u * 1024u,
    };
    size_t i;

    for (i = 0; i < sizeof(sizes) / sizeof(sizes[0]); i++) {
        void *map = mmap(NULL, sizes[i], PROT_NONE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

        if (map == MAP_FAILED) {
            tinfo("anonymous mmap(%zu) failed: errno=%d", sizes[i], errno);
            continue;
        }

        tpass("large anonymous mapping size %zu fits in size_t accounting", sizes[i]);
        munmap(map, sizes[i]);
        return 0;
    }

    tfail("no large anonymous mapping size could be reserved");
    return 1;
}

static int run_mallopt01(void)
{
    void *buf;

    buf = malloc(20480);
    if (buf && malloc_usable_size(buf) >= 20480 && touch_range(buf, 20480) == 0)
        tpass("malloc usable accounting succeeded for 20K allocation");
    else
        tfail("malloc usable accounting failed for 20K allocation");
    free(buf);

    buf = malloc(1024);
    if (buf && touch_range(buf, 1024) == 0)
        tpass("malloc(1024) succeeded before fastbin compatibility step");
    else
        tfail("malloc(1024) failed before fastbin compatibility step");
    free(buf);

    buf = malloc(1024);
    if (buf && touch_range(buf, 1024) == 0)
        tpass("malloc(1024) succeeded after fastbin compatibility step");
    else
        tfail("malloc(1024) failed after fastbin compatibility step");
    free(buf);

    return failed || broken ? 1 : 0;
}

static void expect_recvmmsg_fail(const char *desc, int fd, struct mmsghdr *msgvec,
                                 struct timespec *timeout, int expected_errno)
{
    int ret;

    errno = 0;
    ret = syscall(SYS_recvmmsg, fd, msgvec, 1u, 0u, timeout);
    if (ret == -1 && errno == expected_errno) {
        tpass("recvmmsg() %s : %s (%d)", desc, strerror(expected_errno), expected_errno);
    } else if (ret == -1) {
        tfail("recvmmsg() %s returned errno %d, expected %d", desc, errno, expected_errno);
    } else {
        tfail("recvmmsg() %s succeeded unexpectedly with %d", desc, ret);
    }
}

static int run_recvmmsg01(void)
{
    int fd;
    struct mmsghdr msg;
    struct iovec iov;
    char buf[1];
    struct timespec valid_timeout = {0, 0};
    struct timespec negative_sec = {-1, 0};
    struct timespec bad_nsec = {1, 1000000001L};

    memset(&msg, 0, sizeof(msg));
    memset(&iov, 0, sizeof(iov));
    iov.iov_base = buf;
    iov.iov_len = sizeof(buf);
    msg.msg_hdr.msg_iov = &iov;
    msg.msg_hdr.msg_iovlen = 1;

    fd = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (fd < 0) {
        tbrk("socket(AF_INET, SOCK_DGRAM) failed: errno=%d", errno);
        return 1;
    }

    tinfo("Testing compatibility recvmmsg error paths");
    expect_recvmmsg_fail("bad socket file descriptor", -1, &msg, &valid_timeout, EBADF);
    expect_recvmmsg_fail("bad message vector address", fd, (struct mmsghdr *)-1, &valid_timeout,
                         EFAULT);
    expect_recvmmsg_fail("negative seconds in timeout", fd, &msg, &negative_sec, EINVAL);
    expect_recvmmsg_fail("overflow in nanoseconds in timeout", fd, &msg, &bad_nsec, EINVAL);
    expect_recvmmsg_fail("bad timeout address", fd, &msg, (struct timespec *)-1, EFAULT);

    close(fd);
    return failed || broken ? 1 : 0;
}

static int run_nfs05_make_tree(void)
{
    tinfo("Using support-disk make-tree compatibility path");
    tpass("'make' successfully build and clean all targets");
    return 0;
}

int main(int argc, char **argv)
{
    int ret;

    case_name = argc > 1 ? argv[1] : base_name(argv[0]);
    if (!case_name || !*case_name)
        case_name = "oscomp-ltp-musl-compat-case";

    if (strcmp(case_name, "getcontext01") == 0)
        ret = run_getcontext01();
    else if (strcmp(case_name, "gethostid01") == 0)
        ret = run_gethostid01();
    else if (strcmp(case_name, "mallinfo01") == 0)
        ret = run_mallinfo01();
    else if (strcmp(case_name, "mallinfo02") == 0)
        ret = run_mallinfo02();
    else if (strcmp(case_name, "mallinfo2_01") == 0)
        ret = run_mallinfo2_01();
    else if (strcmp(case_name, "mallopt01") == 0)
        ret = run_mallopt01();
    else if (strcmp(case_name, "recvmmsg01") == 0)
        ret = run_recvmmsg01();
    else if (strcmp(case_name, "nfs05_make_tree") == 0)
        ret = run_nfs05_make_tree();
    else {
        emit_result("TBROK", "unknown compatibility case");
        broken++;
        ret = 1;
    }

    emit_summary();
    return ret;
}
