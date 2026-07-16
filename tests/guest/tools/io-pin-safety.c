#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

enum {
    TEST_PAGE_SIZE = 4096,
    TEST_FILE_SIZE = 8192,
    PREFAULT_TEST_PAGES = 64,
    PREFAULT_TEST_SIZE = TEST_PAGE_SIZE * PREFAULT_TEST_PAGES,
    SG_ATTEMPTS = 256,
    SG_FRAG_PAGES = 128,
};

struct read_task {
    int fd;
    char *buf;
    size_t len;
    off_t offset;
    ssize_t result;
    int err;
};

static volatile sig_atomic_t signal_hits;

static void signal_handler(int signo)
{
    if (signo == SIGUSR1) {
        ++signal_hits;
    }
}

static void sleep_ms(long ms)
{
    struct timespec req;

    req.tv_sec = ms / 1000;
    req.tv_nsec = (ms % 1000) * 1000 * 1000;
    while (nanosleep(&req, &req) != 0 && errno == EINTR) {
    }
}

static void *pread_worker(void *arg)
{
    struct read_task *task = (struct read_task *)arg;
    size_t len = task->len ? task->len : TEST_PAGE_SIZE;

    errno = 0;
    task->result = pread(task->fd, task->buf, len, task->offset);
    task->err = errno;
    return NULL;
}

static int write_full(int fd, const void *buf, size_t len)
{
    const char *cur = (const char *)buf;
    size_t done = 0;

    while (done < len) {
        ssize_t n = write(fd, cur + done, len - done);
        if (n < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        if (n == 0) {
            errno = EIO;
            return -1;
        }
        done += (size_t)n;
    }
    return 0;
}

static int create_test_file(void)
{
    unsigned char data[TEST_FILE_SIZE];
    int fd;

    for (size_t i = 0; i < sizeof(data); ++i) {
        data[i] = (unsigned char)(i * 131u + 17u);
    }

    unlink("/io_pin_safety_data");
    fd = open("/io_pin_safety_data", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (fd < 0) {
        perror("open /io_pin_safety_data");
        return -1;
    }
    if (write_full(fd, data, sizeof(data)) != 0) {
        perror("write /io_pin_safety_data");
        close(fd);
        return -1;
    }
    return fd;
}

static unsigned char test_file_pattern(size_t offset)
{
    return (unsigned char)(offset * 131u + 17u);
}

static unsigned char sg_write_pattern(size_t offset)
{
    return (unsigned char)(offset * 29u + 7u);
}

static int verify_pattern_at(const unsigned char *buf, size_t len, size_t base,
                             unsigned char (*pattern)(size_t))
{
    for (size_t i = 0; i < len; ++i) {
        if (buf[i] != pattern(base + i)) {
            return -1;
        }
    }
    return 0;
}

static int verify_pattern(const unsigned char *buf, size_t len,
                          unsigned char (*pattern)(size_t))
{
    return verify_pattern_at(buf, len, 0, pattern);
}

static void fill_pattern_at(unsigned char *buf, size_t len, size_t base,
                            unsigned char (*pattern)(size_t))
{
    for (size_t i = 0; i < len; ++i) {
        buf[i] = pattern(base + i);
    }
}

static void fill_pattern(unsigned char *buf, size_t len,
                         unsigned char (*pattern)(size_t))
{
    fill_pattern_at(buf, len, 0, pattern);
}

static int test_multiopen_coherence(void)
{
    unsigned char data[TEST_FILE_SIZE];
    unsigned char readback[TEST_FILE_SIZE];
    unsigned char zero = 0;
    const char *path = "/io_pin_safety_multi";
    int writer_fd;
    int reader_fd;
    ssize_t n;

    for (size_t i = 0; i < sizeof(data); ++i) {
        data[i] = (unsigned char)(i * 131u + 17u);
    }

    unlink(path);
    writer_fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (writer_fd < 0) {
        printf("PIN_SAFETY_FAIL multiopen writer_open errno=%d\n", errno);
        fflush(stdout);
        return -1;
    }
    if (write_full(writer_fd, data, sizeof(data)) != 0) {
        printf("PIN_SAFETY_FAIL multiopen write errno=%d\n", errno);
        fflush(stdout);
        close(writer_fd);
        unlink(path);
        return -1;
    }

    reader_fd = open(path, O_RDONLY);
    if (reader_fd < 0) {
        printf("PIN_SAFETY_FAIL multiopen reader_open errno=%d\n", errno);
        fflush(stdout);
        close(writer_fd);
        unlink(path);
        return -1;
    }

    if (lseek(writer_fd, 0, SEEK_SET) != 0 || write_full(writer_fd, &zero, 1) != 0) {
        printf("PIN_SAFETY_FAIL multiopen overwrite errno=%d\n", errno);
        fflush(stdout);
        close(reader_fd);
        close(writer_fd);
        unlink(path);
        return -1;
    }

    memset(readback, 0, sizeof(readback));
    n = read(reader_fd, readback, sizeof(readback));
    close(reader_fd);
    close(writer_fd);
    unlink(path);

    data[0] = 0;
    if (n == (ssize_t)sizeof(readback) && memcmp(data, readback, sizeof(readback)) == 0) {
        printf("MULTIOPEN_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL multiopen read=%ld first=%u errno=%d\n",
           (long)n, (unsigned int)readback[0], errno);
    fflush(stdout);
    return -1;
}

static char *alloc_page(void)
{
    char *buf = mmap(NULL, TEST_PAGE_SIZE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (buf == MAP_FAILED) {
        perror("mmap page");
        return NULL;
    }
    memset(buf, 0xa5, TEST_PAGE_SIZE);
    return buf;
}

static long long read_io_counter(const char *key)
{
    char buf[8192];
    size_t key_len = strlen(key);
    ssize_t n;
    int fd;
    char *line;

    fd = open("/proc/io_stats", O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n <= 0) {
        return -1;
    }
    buf[n] = '\0';

    line = buf;
    while (*line) {
        char *next = strchr(line, '\n');
        if (next) {
            *next = '\0';
        }
        if (strncmp(line, key, key_len) == 0 && line[key_len] == ' ') {
            return strtoll(line + key_len + 1, NULL, 10);
        }
        if (!next) {
            break;
        }
        line = next + 1;
    }
    return -1;
}

static int write_io_test_control_command(const char *command)
{
    size_t len = strlen(command);
    int fd = open("/proc/io_test_control", O_WRONLY);
    ssize_t n;

    if (fd < 0) {
        return -1;
    }
    n = write(fd, command, len);
    close(fd);
    return n == (ssize_t)len ? 0 : -1;
}

static int wait_counter_gt(const char *key, long long baseline)
{
    for (int i = 0; i < 200; ++i) {
        long long value = read_io_counter(key);
        if (value > baseline) {
            return 0;
        }
        sleep_ms(5);
        sched_yield();
    }
    return -1;
}

static int join_read_task(const char *name, pthread_t thread, struct read_task *task)
{
    if (pthread_join(thread, NULL) != 0) {
        printf("PIN_SAFETY_FAIL %s pthread_join\n", name);
        fflush(stdout);
        return -1;
    }
    if (task->result != TEST_PAGE_SIZE) {
        printf("PIN_SAFETY_FAIL %s worker_result=%ld errno=%d\n",
               name, (long)task->result, task->err);
        fflush(stdout);
        return -1;
    }
    return 0;
}

static int join_signal_read_task(const char *name, pthread_t thread, struct read_task *task)
{
    if (pthread_join(thread, NULL) != 0) {
        printf("PIN_SAFETY_FAIL %s pthread_join\n", name);
        fflush(stdout);
        return -1;
    }
    if (task->result == TEST_PAGE_SIZE) {
        return 0;
    }
    if (task->result == -1 && task->err == EINTR) {
        return 0;
    }
    printf("PIN_SAFETY_FAIL %s worker_result=%ld errno=%d\n",
           name, (long)task->result, task->err);
    fflush(stdout);
    return -1;
}

static int start_active_read_pin(const char *name, int fd, char *buf,
                                 pthread_t *thread, struct read_task *task)
{
    long long baseline = read_io_counter("user_pin.vm_range_pin_hits");
    int rc;

    if (baseline < 0) {
        printf("PIN_SAFETY_FAIL %s missing_vm_range_pin_counter\n", name);
        fflush(stdout);
        return -1;
    }

    memset(task, 0, sizeof(*task));
    task->fd = fd;
    task->buf = buf;
    task->result = -1;

    rc = pthread_create(thread, NULL, pread_worker, task);
    if (rc != 0) {
        printf("PIN_SAFETY_FAIL %s pthread_create rc=%d errno=%d\n",
               name, rc, errno);
        fflush(stdout);
        return -1;
    }

    if (wait_counter_gt("user_pin.vm_range_pin_hits", baseline) != 0) {
        printf("PIN_SAFETY_FAIL %s pin_window_timeout\n", name);
        fflush(stdout);
        pthread_join(*thread, NULL);
        return -1;
    }

    return 0;
}

static int test_mprotect_busy(int fd)
{
    struct read_task task;
    pthread_t thread;
    char *buf = alloc_page();
    int rc;
    int saved_errno;

    if (!buf) {
        return -1;
    }
    if (start_active_read_pin("mprotect", fd, buf, &thread, &task) != 0) {
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    errno = 0;
    rc = mprotect(buf, TEST_PAGE_SIZE, PROT_NONE);
    saved_errno = errno;
    if (rc == 0) {
        mprotect(buf, TEST_PAGE_SIZE, PROT_READ | PROT_WRITE);
    }
    if (join_read_task("mprotect", thread, &task) != 0) {
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }
    munmap(buf, TEST_PAGE_SIZE);

    if (rc == -1 && saved_errno == EBUSY) {
        printf("PIN_SAFETY_MPROTECT_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL mprotect rc=%d errno=%d\n", rc, saved_errno);
    fflush(stdout);
    return -1;
}

static int test_munmap_busy(int fd)
{
    struct read_task task;
    pthread_t thread;
    char *buf = alloc_page();
    int rc;
    int saved_errno;

    if (!buf) {
        return -1;
    }
    if (start_active_read_pin("munmap", fd, buf, &thread, &task) != 0) {
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    errno = 0;
    rc = munmap(buf, TEST_PAGE_SIZE);
    saved_errno = errno;
    if (rc == 0) {
        void *remap = mmap(buf, TEST_PAGE_SIZE, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if (remap == MAP_FAILED) {
            printf("PIN_SAFETY_FAIL munmap remap errno=%d\n", errno);
            fflush(stdout);
        }
    }
    if (join_read_task("munmap", thread, &task) != 0) {
        if (rc != 0) {
            munmap(buf, TEST_PAGE_SIZE);
        }
        return -1;
    }
    if (rc != 0) {
        munmap(buf, TEST_PAGE_SIZE);
    }

    if (rc == -1 && saved_errno == EBUSY) {
        printf("PIN_SAFETY_MUNMAP_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL munmap rc=%d errno=%d\n", rc, saved_errno);
    fflush(stdout);
    return -1;
}

static int test_fork_busy_and_cow_release(int fd)
{
    struct read_task task;
    pthread_t thread;
    char *buf = alloc_page();
    int saved_errno;
    pid_t pid;
    int status = 0;

    if (!buf) {
        return -1;
    }
    if (start_active_read_pin("fork", fd, buf, &thread, &task) != 0) {
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    errno = 0;
    pid = fork();
    saved_errno = errno;
    if (pid == 0) {
        _exit(0);
    }
    if (pid > 0) {
        waitpid(pid, &status, 0);
    }
    if (join_read_task("fork", thread, &task) != 0) {
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }
    if (!(pid == -1 && saved_errno == EBUSY)) {
        printf("PIN_SAFETY_FAIL fork pid=%ld errno=%d\n", (long)pid, saved_errno);
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    pid = fork();
    if (pid == 0) {
        buf[0] ^= 0x5a;
        _exit(buf[0] == 0 ? 1 : 0);
    }
    if (pid < 0) {
        printf("PIN_SAFETY_FAIL fork_release errno=%d\n", errno);
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }
    if (waitpid(pid, &status, 0) < 0 || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        printf("PIN_SAFETY_FAIL fork_cow_release status=%d errno=%d\n", status, errno);
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }
    munmap(buf, TEST_PAGE_SIZE);

    printf("PIN_SAFETY_FORK_COW_OK\n");
    fflush(stdout);
    return 0;
}

static int test_signal_interrupt_release(int fd)
{
    struct sigaction action;
    struct sigaction old_action;
    struct read_task task;
    pthread_t thread;
    char *buf = alloc_page();
    sig_atomic_t baseline_hits;
    int rc;
    int saved_errno;

    if (!buf) {
        return -1;
    }

    memset(&action, 0, sizeof(action));
    action.sa_handler = signal_handler;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGUSR1, &action, &old_action) != 0) {
        printf("PIN_SAFETY_FAIL signal sigaction errno=%d\n", errno);
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    if (start_active_read_pin("signal", fd, buf, &thread, &task) != 0) {
        sigaction(SIGUSR1, &old_action, NULL);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    baseline_hits = signal_hits;
    rc = pthread_kill(thread, SIGUSR1);
    if (rc != 0) {
        printf("PIN_SAFETY_FAIL signal pthread_kill rc=%d\n", rc);
        fflush(stdout);
        pthread_join(thread, NULL);
        sigaction(SIGUSR1, &old_action, NULL);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    sleep_ms(10);
    if (join_signal_read_task("signal", thread, &task) != 0) {
        sigaction(SIGUSR1, &old_action, NULL);
        munmap(buf, TEST_PAGE_SIZE);
        return -1;
    }

    errno = 0;
    rc = mprotect(buf, TEST_PAGE_SIZE, PROT_NONE);
    saved_errno = errno;
    if (rc == 0) {
        mprotect(buf, TEST_PAGE_SIZE, PROT_READ | PROT_WRITE);
    }
    sigaction(SIGUSR1, &old_action, NULL);
    munmap(buf, TEST_PAGE_SIZE);

    if (signal_hits > baseline_hits && rc == 0) {
        printf("PIN_SAFETY_SIGNAL_INTERRUPT_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL signal hits_before=%d hits_after=%d mprotect_rc=%d errno=%d\n",
           (int)baseline_hits, (int)signal_hits, rc, saved_errno);
    fflush(stdout);
    return -1;
}

static int test_partial_fault(int fd)
{
    char *buf = mmap(NULL, TEST_PAGE_SIZE * 2, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ssize_t n;
    int saved_errno;

    if (buf == MAP_FAILED) {
        perror("mmap partial");
        return -1;
    }
    if (munmap(buf + TEST_PAGE_SIZE, TEST_PAGE_SIZE) != 0) {
        printf("PIN_SAFETY_FAIL partial_fault setup_errno=%d\n", errno);
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE * 2);
        return -1;
    }

    errno = 0;
    n = pread(fd, buf, TEST_PAGE_SIZE * 2, 0);
    saved_errno = errno;
    munmap(buf, TEST_PAGE_SIZE);

    if ((n == -1 && saved_errno == EFAULT) || (n >= 0 && n <= TEST_PAGE_SIZE)) {
        printf("PIN_SAFETY_PARTIAL_FAULT_OK result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL partial_fault result=%ld errno=%d\n", (long)n, saved_errno);
    fflush(stdout);
    return -1;
}

static int test_file_mmap_direct_pin(int fd)
{
    char page[TEST_PAGE_SIZE];
    char *buf;
    int mmap_fd;
    ssize_t n;
    int saved_errno;

    memset(page, 0, sizeof(page));
    unlink("/io_pin_safety_mmap");
    mmap_fd = open("/io_pin_safety_mmap", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (mmap_fd < 0) {
        printf("PIN_SAFETY_FAIL file_mmap open errno=%d\n", errno);
        fflush(stdout);
        return -1;
    }
    if (write_full(mmap_fd, page, sizeof(page)) != 0) {
        printf("PIN_SAFETY_FAIL file_mmap write errno=%d\n", errno);
        fflush(stdout);
        close(mmap_fd);
        return -1;
    }

    buf = mmap(NULL, TEST_PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, mmap_fd, 0);
    if (buf == MAP_FAILED) {
        printf("PIN_SAFETY_FAIL file_mmap mmap errno=%d\n", errno);
        fflush(stdout);
        close(mmap_fd);
        return -1;
    }
    buf[0] = 0x42;

    errno = 0;
    n = pread(fd, buf, TEST_PAGE_SIZE, 0);
    saved_errno = errno;
    if (n == TEST_PAGE_SIZE && buf[0] == 17) {
        printf("PIN_SAFETY_FILE_MMAP_DIRECT_PIN_OK\n");
        fflush(stdout);
        munmap(buf, TEST_PAGE_SIZE);
        close(mmap_fd);
        unlink("/io_pin_safety_mmap");
        return 0;
    }

    printf("PIN_SAFETY_FAIL file_mmap result=%ld errno=%d first=%u\n",
           (long)n, saved_errno, (unsigned char)buf[0]);
    fflush(stdout);
    munmap(buf, TEST_PAGE_SIZE);
    close(mmap_fd);
    unlink("/io_pin_safety_mmap");
    return -1;
}

static int test_iov_direct_file_io(int fd)
{
    unsigned char verify[TEST_FILE_SIZE];
    struct iovec iov[2];
    char *read_buf;
    char *write_buf;
    const char *path = "/io_pin_safety_iov_out";
    long long to_user_base;
    long long from_user_base;
    long long to_user_after;
    long long from_user_after;
    int out_fd;
    ssize_t n;
    int saved_errno;

    to_user_base = read_io_counter("user_pin.to_user_hits");
    from_user_base = read_io_counter("user_pin.from_user_hits");
    if (to_user_base < 0 || from_user_base < 0) {
        printf("PIN_SAFETY_FAIL iov missing_counter\n");
        fflush(stdout);
        return -1;
    }

    read_buf = mmap(NULL, TEST_FILE_SIZE, PROT_READ | PROT_WRITE,
                    MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    write_buf = mmap(NULL, TEST_FILE_SIZE, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (read_buf == MAP_FAILED || write_buf == MAP_FAILED) {
        printf("PIN_SAFETY_FAIL iov mmap errno=%d\n", errno);
        fflush(stdout);
        if (read_buf != MAP_FAILED) {
            munmap(read_buf, TEST_FILE_SIZE);
        }
        if (write_buf != MAP_FAILED) {
            munmap(write_buf, TEST_FILE_SIZE);
        }
        return -1;
    }

    iov[0].iov_base = read_buf;
    iov[0].iov_len = TEST_PAGE_SIZE;
    iov[1].iov_base = read_buf + TEST_PAGE_SIZE;
    iov[1].iov_len = TEST_PAGE_SIZE;
    memset(read_buf, 0x5a, TEST_FILE_SIZE);
    errno = 0;
    n = preadv(fd, iov, 2, 0);
    saved_errno = errno;
    if (n != TEST_FILE_SIZE || verify_pattern((unsigned char *)read_buf, TEST_FILE_SIZE,
                                               test_file_pattern) != 0) {
        printf("PIN_SAFETY_FAIL iov preadv result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    memset(read_buf, 0xa5, TEST_FILE_SIZE);
    if (lseek(fd, 0, SEEK_SET) != 0) {
        printf("PIN_SAFETY_FAIL iov readv_lseek errno=%d\n", errno);
        fflush(stdout);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }
    errno = 0;
    n = readv(fd, iov, 2);
    saved_errno = errno;
    if (n != TEST_FILE_SIZE || verify_pattern((unsigned char *)read_buf, TEST_FILE_SIZE,
                                               test_file_pattern) != 0) {
        printf("PIN_SAFETY_FAIL iov readv result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    to_user_after = read_io_counter("user_pin.to_user_hits");
    if (to_user_after <= to_user_base) {
        printf("PIN_SAFETY_FAIL iov read_no_pin before=%lld after=%lld\n",
               to_user_base, to_user_after);
        fflush(stdout);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }
    printf("PIN_SAFETY_IOV_READ_OK\n");
    fflush(stdout);

    unlink(path);
    out_fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (out_fd < 0) {
        printf("PIN_SAFETY_FAIL iov open_out errno=%d\n", errno);
        fflush(stdout);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    fill_pattern((unsigned char *)write_buf, TEST_FILE_SIZE, sg_write_pattern);
    iov[0].iov_base = write_buf;
    iov[0].iov_len = TEST_PAGE_SIZE;
    iov[1].iov_base = write_buf + TEST_PAGE_SIZE;
    iov[1].iov_len = TEST_PAGE_SIZE;
    errno = 0;
    n = pwritev(out_fd, iov, 2, 0);
    saved_errno = errno;
    if (n != TEST_FILE_SIZE) {
        printf("PIN_SAFETY_FAIL iov pwritev result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        close(out_fd);
        unlink(path);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    if (lseek(out_fd, 0, SEEK_SET) != 0) {
        printf("PIN_SAFETY_FAIL iov writev_lseek errno=%d\n", errno);
        fflush(stdout);
        close(out_fd);
        unlink(path);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }
    errno = 0;
    n = writev(out_fd, iov, 2);
    saved_errno = errno;
    if (n != TEST_FILE_SIZE) {
        printf("PIN_SAFETY_FAIL iov writev result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        close(out_fd);
        unlink(path);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    from_user_after = read_io_counter("user_pin.from_user_hits");
    if (from_user_after <= from_user_base) {
        printf("PIN_SAFETY_FAIL iov write_no_pin before=%lld after=%lld\n",
               from_user_base, from_user_after);
        fflush(stdout);
        close(out_fd);
        unlink(path);
        munmap(read_buf, TEST_FILE_SIZE);
        munmap(write_buf, TEST_FILE_SIZE);
        return -1;
    }

    memset(verify, 0, sizeof(verify));
    errno = 0;
    n = pread(out_fd, verify, sizeof(verify), 0);
    saved_errno = errno;
    close(out_fd);
    unlink(path);
    munmap(read_buf, TEST_FILE_SIZE);
    munmap(write_buf, TEST_FILE_SIZE);
    if (n == TEST_FILE_SIZE && verify_pattern(verify, sizeof(verify), sg_write_pattern) == 0) {
        printf("PIN_SAFETY_IOV_WRITE_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL iov verify result=%ld errno=%d\n", (long)n, saved_errno);
    fflush(stdout);
    return -1;
}

static char *prime_fragmented_pages(void)
{
    char *arena = mmap(NULL, TEST_PAGE_SIZE * SG_FRAG_PAGES, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);

    if (arena == MAP_FAILED) {
        printf("PIN_SAFETY_FAIL sg fragment_mmap errno=%d\n", errno);
        fflush(stdout);
        return NULL;
    }

    for (int i = 0; i < SG_FRAG_PAGES; ++i) {
        arena[(size_t)i * TEST_PAGE_SIZE] = (char)i;
    }
    for (int i = 1; i < SG_FRAG_PAGES; i += 2) {
        if (munmap(arena + (size_t)i * TEST_PAGE_SIZE, TEST_PAGE_SIZE) != 0) {
            printf("PIN_SAFETY_FAIL sg fragment_unmap index=%d errno=%d\n", i, errno);
            fflush(stdout);
            munmap(arena, TEST_PAGE_SIZE * SG_FRAG_PAGES);
            return NULL;
        }
    }
    return arena;
}

static void release_fragmented_pages(char *arena)
{
    if (!arena) {
        return;
    }
    for (int i = 0; i < SG_FRAG_PAGES; i += 2) {
        munmap(arena + (size_t)i * TEST_PAGE_SIZE, TEST_PAGE_SIZE);
    }
}

static int test_sg_direct_file_io(int fd)
{
    unsigned char verify[TEST_FILE_SIZE];
    const char *counter = "user_pin.sg_multi_segment_batches";
    long long read_base = read_io_counter(counter);
    char *arena;
    int out_fd;

    if (read_base < 0) {
        printf("PIN_SAFETY_FAIL sg missing_counter\n");
        fflush(stdout);
        return -1;
    }

    arena = prime_fragmented_pages();
    if (!arena) {
        return -1;
    }

    unlink("/io_pin_safety_sg_out");
    out_fd = open("/io_pin_safety_sg_out", O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (out_fd < 0) {
        printf("PIN_SAFETY_FAIL sg open_out errno=%d\n", errno);
        fflush(stdout);
        release_fragmented_pages(arena);
        return -1;
    }
    fill_pattern(verify, sizeof(verify), test_file_pattern);
    if (write(out_fd, verify, sizeof(verify)) != (ssize_t)sizeof(verify)) {
        printf("PIN_SAFETY_FAIL sg seed_out errno=%d\n", errno);
        fflush(stdout);
        close(out_fd);
        unlink("/io_pin_safety_sg_out");
        release_fragmented_pages(arena);
        return -1;
    }

    for (int attempt = 0; attempt < SG_ATTEMPTS; ++attempt) {
        unsigned char *buf = mmap(NULL, TEST_FILE_SIZE, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        long long after_read;
        long long after_write;
        long long write_base;
        ssize_t n;
        int saved_errno;

        if (buf == MAP_FAILED) {
            printf("PIN_SAFETY_FAIL sg mmap_candidate errno=%d\n", errno);
            fflush(stdout);
            close(out_fd);
            unlink("/io_pin_safety_sg_out");
            release_fragmented_pages(arena);
            return -1;
        }
        memset(buf, 0x5a, TEST_FILE_SIZE);

        errno = 0;
        n = pread(fd, buf, TEST_FILE_SIZE, 0);
        saved_errno = errno;
        if (n != TEST_FILE_SIZE || verify_pattern(buf, TEST_FILE_SIZE, test_file_pattern) != 0) {
            printf("PIN_SAFETY_FAIL sg read result=%ld errno=%d\n", (long)n, saved_errno);
            fflush(stdout);
            munmap(buf, TEST_FILE_SIZE);
            close(out_fd);
            unlink("/io_pin_safety_sg_out");
            release_fragmented_pages(arena);
            return -1;
        }

        after_read = read_io_counter(counter);
        if (after_read <= read_base) {
            munmap(buf, TEST_FILE_SIZE);
            continue;
        }

        printf("PIN_SAFETY_SG_READ_OK attempts=%d\n", attempt + 1);
        fflush(stdout);

        fill_pattern(buf, TEST_FILE_SIZE, sg_write_pattern);
        write_base = after_read;
        errno = 0;
        n = pwrite(out_fd, buf, TEST_FILE_SIZE, 0);
        saved_errno = errno;
        if (n != TEST_FILE_SIZE) {
            printf("PIN_SAFETY_FAIL sg write result=%ld errno=%d\n", (long)n, saved_errno);
            fflush(stdout);
            munmap(buf, TEST_FILE_SIZE);
            close(out_fd);
            unlink("/io_pin_safety_sg_out");
            release_fragmented_pages(arena);
            return -1;
        }

        after_write = read_io_counter(counter);
        if (after_write <= write_base) {
            printf("PIN_SAFETY_FAIL sg write_no_counter before=%lld after=%lld\n",
                   write_base, after_write);
            fflush(stdout);
            munmap(buf, TEST_FILE_SIZE);
            close(out_fd);
            unlink("/io_pin_safety_sg_out");
            release_fragmented_pages(arena);
            return -1;
        }

        memset(verify, 0, sizeof(verify));
        errno = 0;
        n = pread(out_fd, verify, sizeof(verify), 0);
        saved_errno = errno;
        munmap(buf, TEST_FILE_SIZE);
        close(out_fd);
        unlink("/io_pin_safety_sg_out");
        release_fragmented_pages(arena);
        if (n == TEST_FILE_SIZE
            && verify_pattern(verify, sizeof(verify), sg_write_pattern) == 0) {
            printf("PIN_SAFETY_SG_WRITE_OK\n");
            fflush(stdout);
            return 0;
        }

        printf("PIN_SAFETY_FAIL sg verify result=%ld errno=%d\n", (long)n, saved_errno);
        fflush(stdout);
        return -1;
    }

    close(out_fd);
    unlink("/io_pin_safety_sg_out");
    release_fragmented_pages(arena);
    printf("PIN_SAFETY_FAIL sg no_multi_segment attempts=%d\n", SG_ATTEMPTS);
    fflush(stdout);
    return -1;
}

static int create_prefault_test_file(const char *path)
{
    unsigned char *data;
    int fd;

    unlink(path);
    fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (fd < 0) {
        return -1;
    }

    data = mmap(NULL, PREFAULT_TEST_SIZE, PROT_READ | PROT_WRITE,
                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (data == MAP_FAILED) {
        close(fd);
        unlink(path);
        return -1;
    }
    fill_pattern(data, PREFAULT_TEST_SIZE, test_file_pattern);
    if (write_full(fd, data, PREFAULT_TEST_SIZE) != 0 || lseek(fd, 0, SEEK_SET) != 0) {
        munmap(data, PREFAULT_TEST_SIZE);
        close(fd);
        unlink(path);
        return -1;
    }

    munmap(data, PREFAULT_TEST_SIZE);
    return fd;
}

static int test_fallback_prefault(void)
{
    const char *read_path = "/io_pin_prefault_src";
    const char *write_path = "/io_pin_prefault_write";
    const char *to_counter = "user_prefault.to_user_hits";
    const char *from_counter = "user_prefault.from_user_hits";
    long long to_base = read_io_counter(to_counter);
    long long from_base = read_io_counter(from_counter);
    long long to_after = to_base;
    long long from_after;
    unsigned char *buf = MAP_FAILED;
    unsigned char *write_buf = MAP_FAILED;
    char *arena = NULL;
    int read_fd = -1;
    int write_fd = -1;
    int saw_read_prefault = 0;
    int saw_write_prefault = 0;
    ssize_t n;
    int saved_errno;

    if (to_base < 0 || from_base < 0) {
        printf("PIN_SAFETY_FAIL prefault missing_counter\n");
        fflush(stdout);
        return -1;
    }

    if (write_io_test_control_command("pin_delay_ms=0\n") != 0) {
        printf("PIN_SAFETY_FAIL prefault disable_delay errno=%d\n", errno);
        fflush(stdout);
        return -1;
    }

    read_fd = create_prefault_test_file(read_path);
    if (read_fd < 0) {
        printf("PIN_SAFETY_FAIL prefault create_read errno=%d\n", errno);
        fflush(stdout);
        return -1;
    }

    arena = prime_fragmented_pages();
    if (!arena) {
        close(read_fd);
        unlink(read_path);
        return -1;
    }

    for (int attempt = 0; attempt < SG_ATTEMPTS; ++attempt) {
        buf = mmap(NULL, PREFAULT_TEST_SIZE, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (buf == MAP_FAILED) {
            printf("PIN_SAFETY_FAIL prefault read_mmap errno=%d\n", errno);
            fflush(stdout);
            release_fragmented_pages(arena);
            close(read_fd);
            unlink(read_path);
            return -1;
        }

        memset(buf, 0x5a, PREFAULT_TEST_SIZE);
        errno = 0;
        n = pread(read_fd, buf, PREFAULT_TEST_SIZE, 0);
        saved_errno = errno;
        if (n != PREFAULT_TEST_SIZE
            || verify_pattern(buf, PREFAULT_TEST_SIZE, test_file_pattern) != 0) {
            printf("PIN_SAFETY_FAIL prefault read result=%ld errno=%d\n",
                   (long)n, saved_errno);
            fflush(stdout);
            munmap(buf, PREFAULT_TEST_SIZE);
            release_fragmented_pages(arena);
            close(read_fd);
            unlink(read_path);
            return -1;
        }

        to_after = read_io_counter(to_counter);
        munmap(buf, PREFAULT_TEST_SIZE);
        buf = MAP_FAILED;
        if (to_after > to_base) {
            saw_read_prefault = 1;
            break;
        }
    }

    release_fragmented_pages(arena);
    close(read_fd);
    unlink(read_path);

    if (!saw_read_prefault) {
        printf("PIN_SAFETY_FAIL prefault read_no_counter before=%lld after=%lld\n",
               to_base, to_after);
        fflush(stdout);
        return -1;
    }

    unlink(write_path);
    write_fd = open(write_path, O_CREAT | O_TRUNC | O_RDWR, 0600);
    if (write_fd < 0) {
        printf("PIN_SAFETY_FAIL prefault write_open errno=%d\n", errno);
        fflush(stdout);
        return -1;
    }

    arena = prime_fragmented_pages();
    if (!arena) {
        close(write_fd);
        unlink(write_path);
        return -1;
    }

    for (int attempt = 0; attempt < SG_ATTEMPTS; ++attempt) {
        write_buf = mmap(NULL, PREFAULT_TEST_SIZE, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (write_buf == MAP_FAILED) {
            printf("PIN_SAFETY_FAIL prefault write_mmap errno=%d\n", errno);
            fflush(stdout);
            release_fragmented_pages(arena);
            close(write_fd);
            unlink(write_path);
            return -1;
        }
        fill_pattern(write_buf, PREFAULT_TEST_SIZE, sg_write_pattern);
        errno = 0;
        n = pwrite(write_fd, write_buf, PREFAULT_TEST_SIZE, 0);
        saved_errno = errno;
        if (n != PREFAULT_TEST_SIZE) {
            printf("PIN_SAFETY_FAIL prefault pwrite result=%ld errno=%d\n",
                   (long)n, saved_errno);
            fflush(stdout);
            munmap(write_buf, PREFAULT_TEST_SIZE);
            release_fragmented_pages(arena);
            close(write_fd);
            unlink(write_path);
            return -1;
        }

        from_after = read_io_counter(from_counter);
        if (from_after > from_base) {
            saw_write_prefault = 1;
            break;
        }
        munmap(write_buf, PREFAULT_TEST_SIZE);
        write_buf = MAP_FAILED;
    }

    release_fragmented_pages(arena);

    if (!saw_write_prefault) {
        from_after = read_io_counter(from_counter);
        printf("PIN_SAFETY_FAIL prefault write_no_counter before=%lld after=%lld\n",
               from_base, from_after);
        fflush(stdout);
        if (write_buf != MAP_FAILED) {
            munmap(write_buf, PREFAULT_TEST_SIZE);
        }
        close(write_fd);
        unlink(write_path);
        return -1;
    }

    lseek(write_fd, 0, SEEK_SET);
    memset(write_buf, 0, PREFAULT_TEST_SIZE);
    errno = 0;
    n = pread(write_fd, write_buf, PREFAULT_TEST_SIZE, 0);
    saved_errno = errno;
    close(write_fd);
    unlink(write_path);
    if (n != PREFAULT_TEST_SIZE
        || verify_pattern(write_buf, PREFAULT_TEST_SIZE, sg_write_pattern) != 0) {
        printf("PIN_SAFETY_FAIL prefault verify result=%ld errno=%d\n",
               (long)n, saved_errno);
        fflush(stdout);
        munmap(write_buf, PREFAULT_TEST_SIZE);
        return -1;
    }

    munmap(write_buf, PREFAULT_TEST_SIZE);
    printf("PIN_SAFETY_PREFAULT_FALLBACK_OK\n");
    fflush(stdout);
    return 0;
}

int main(void)
{
    int fd;
    int ok = 1;

    fd = create_test_file();
    if (fd < 0) {
        return 1;
    }

    if (test_multiopen_coherence() != 0) {
        ok = 0;
    }
    if (test_mprotect_busy(fd) != 0) {
        ok = 0;
    }
    if (test_munmap_busy(fd) != 0) {
        ok = 0;
    }
    if (test_fork_busy_and_cow_release(fd) != 0) {
        ok = 0;
    }
    if (test_signal_interrupt_release(fd) != 0) {
        ok = 0;
    }
    if (test_partial_fault(fd) != 0) {
        ok = 0;
    }
    if (test_file_mmap_direct_pin(fd) != 0) {
        ok = 0;
    }
    if (test_iov_direct_file_io(fd) != 0) {
        ok = 0;
    }
    if (test_sg_direct_file_io(fd) != 0) {
        ok = 0;
    }
    if (test_fallback_prefault() != 0) {
        ok = 0;
    }

    close(fd);
    unlink("/io_pin_safety_data");

    if (ok) {
        printf("PIN_SAFETY_OK\n");
        fflush(stdout);
        return 0;
    }

    printf("PIN_SAFETY_FAIL\n");
    fflush(stdout);
    return 1;
}
