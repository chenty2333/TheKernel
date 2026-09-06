#define _GNU_SOURCE

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define POLL_ATTEMPTS 300
#define POLL_DELAY_NS 10000000L
#define PID_REUSE_ATTEMPTS 128

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_PROC_ZOMBIE_FAIL %s errno=%d (%s)\n", stage,
            errno, strerror(errno));
    return 1;
}

static void poll_delay(void)
{
    struct timespec delay = {
        .tv_sec = 0,
        .tv_nsec = POLL_DELAY_NS,
    };

    while (nanosleep(&delay, &delay) != 0 && errno == EINTR) {
    }
}

static int proc_root_contains(pid_t pid)
{
    DIR *directory = opendir("/proc");
    if (directory == NULL) {
        int saved_errno = errno;
        fprintf(stderr, "proc-root stage=opendir errno=%d\n", saved_errno);
        errno = saved_errno;
        return -1;
    }

    char expected[32];
    int length = snprintf(expected, sizeof(expected), "%ld", (long)pid);
    if (length < 0 || (size_t)length >= sizeof(expected)) {
        int saved_errno = errno;
        closedir(directory);
        errno = saved_errno == 0 ? EOVERFLOW : saved_errno;
        return -1;
    }

    int found = 0;
    struct dirent *entry;
    errno = 0;
    while ((entry = readdir(directory)) != NULL) {
        if (strcmp(entry->d_name, expected) == 0) {
            found = 1;
            break;
        }
    }
    int saved_errno = errno;
    if (closedir(directory) != 0)
        return -1;
    if (entry == NULL && saved_errno != 0) {
        fprintf(stderr, "proc-root stage=readdir errno=%d\n", saved_errno);
        errno = saved_errno;
        return -1;
    }
    return found;
}

static int proc_stat_open(pid_t pid)
{
    char path[64];
    int length = snprintf(path, sizeof(path), "/proc/%ld/stat", (long)pid);
    if (length < 0 || (size_t)length >= sizeof(path)) {
        errno = EOVERFLOW;
        return -1;
    }
    return open(path, O_RDONLY | O_CLOEXEC);
}

static int proc_dir_open(pid_t pid)
{
    char path[64];
    int length = snprintf(path, sizeof(path), "/proc/%ld", (long)pid);
    if (length < 0 || (size_t)length >= sizeof(path)) {
        errno = EOVERFLOW;
        return -1;
    }
    return open(path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
}

static int proc_stat_openat(int dirfd)
{
    return openat(dirfd, "stat", O_RDONLY | O_CLOEXEC);
}

static int proc_stat_read_details(int fd, char *state,
                                  unsigned long long *start_time)
{
    char buffer[4096];
    ssize_t count = read(fd, buffer, sizeof(buffer) - 1);
    if (count < 0)
        return -1;
    if (count == 0) {
        errno = EPROTO;
        return -1;
    }
    buffer[count] = '\0';

    char *comm_end = strrchr(buffer, ')');
    if (comm_end == NULL || comm_end[1] != ' ' || comm_end[2] == '\0') {
        errno = EPROTO;
        return -1;
    }

    char *cursor = comm_end + 2;
    for (unsigned int field = 3; field <= 22; field++) {
        while (*cursor == ' ' || *cursor == '\t')
            cursor++;
        if (*cursor == '\0') {
            errno = EPROTO;
            return -1;
        }
        if (field == 3) {
            *state = *cursor++;
            if (*cursor != ' ' && *cursor != '\t' && *cursor != '\0') {
                errno = EPROTO;
                return -1;
            }
            continue;
        }

        char *end = NULL;
        errno = 0;
        unsigned long long value = strtoull(cursor, &end, 10);
        if (end == cursor || errno == ERANGE) {
            errno = EPROTO;
            return -1;
        }
        if (field == 22 && start_time != NULL)
            *start_time = value;
        cursor = end;
    }
    return 1;
}

static int proc_stat_read(int fd, char *state)
{
    return proc_stat_read_details(fd, state, NULL);
}

/* Returns 1 with STATE and START_TIME set, 0 when the proc stat file is
 * absent, or -1. START_TIME is field 22 from /proc/<pid>/stat. */
static int proc_stat_identity(pid_t pid, char *state,
                              unsigned long long *start_time)
{
    int fd = proc_stat_open(pid);
    if (fd < 0) {
        if (errno == ENOENT)
            return 0;
        return -1;
    }

    int result = proc_stat_read_details(fd, state, start_time);
    int saved_errno = errno;
    if (close(fd) != 0)
        return -1;
    if (result < 0)
        errno = saved_errno;
    return result;
}

/* Returns 1 with STATE set, 0 when the proc stat file is absent, or -1. */
static int proc_stat_state(pid_t pid, char *state)
{
    return proc_stat_identity(pid, state, NULL);
}

static int wait_for_zombie(pid_t child)
{
    for (int attempt = 0; attempt < POLL_ATTEMPTS; attempt++) {
        int present = proc_root_contains(child);
        if (present < 0)
            return fail("zombie-root-enumeration");
        char state = '\0';
        int stat_result = proc_stat_state(child, &state);
        if (stat_result < 0)
            return fail("zombie-stat-read");
        if (present == 1 && stat_result == 1 && state == 'Z')
            return 0;
        poll_delay();
    }
    errno = ETIMEDOUT;
    return fail("zombie-appearance-timeout");
}

static int wait_for_reap_visibility_to_clear(pid_t child)
{
    for (int attempt = 0; attempt < POLL_ATTEMPTS; attempt++) {
        int present = proc_root_contains(child);
        if (present < 0)
            return fail("reaped-root-enumeration");
        char state = '\0';
        int stat_result = proc_stat_state(child, &state);
        if (stat_result < 0)
            return fail("reaped-stat-read");
        if (present == 0 && stat_result == 0)
            return 0;
        poll_delay();
    }
    errno = ETIMEDOUT;
    return fail("reaped-disappearance-timeout");
}

int main(void)
{
    pid_t child = fork();
    if (child < 0)
        return fail("fork");
    if (child == 0)
        _exit(42);

    if (wait_for_zombie(child) != 0)
        return 1;
    puts("THEKERNEL_PROC_ZOMBIE_ROOT_ENUM_OK");
    puts("THEKERNEL_PROC_ZOMBIE_STAT_Z_OK");

    int proc_dirfd = proc_dir_open(child);
    if (proc_dirfd < 0)
        return fail("open-dir-before-reap");
    struct stat old_dir_identity;
    if (fstat(proc_dirfd, &old_dir_identity) != 0) {
        close(proc_dirfd);
        return fail("stat-dir-before-reap");
    }
    if (!S_ISDIR(old_dir_identity.st_mode)) {
        errno = ENOTDIR;
        close(proc_dirfd);
        return fail("dir-identity-before-reap");
    }
    char state = '\0';
    unsigned long long old_start_time = 0;
    if (proc_stat_identity(child, &state, &old_start_time) != 1 ||
        state != 'Z') {
        errno = EPROTO;
        close(proc_dirfd);
        return fail("stat-identity-before-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_DIRFD_OPEN_BEFORE_REAP_OK");

    int open_stat = proc_stat_open(child);
    if (open_stat < 0) {
        close(proc_dirfd);
        return fail("open-before-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_OPEN_BEFORE_REAP_OK");

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        close(open_stat);
        close(proc_dirfd);
        return fail("waitpid");
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 42) {
        errno = EPROTO;
        close(open_stat);
        close(proc_dirfd);
        return fail("wait-status");
    }
    puts("THEKERNEL_PROC_ZOMBIE_REAP_OK");

    errno = 0;
    int open_read = proc_stat_read(open_stat, &state);
    int open_read_errno = errno;
    if (open_read != -1 || open_read_errno != ESRCH) {
        errno = open_read_errno == 0 ? EPROTO : open_read_errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("open-stat-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_OPEN_AFTER_REAP_ESRCH_OK");

    struct stat retained_dir_identity;
    if (fstat(proc_dirfd, &retained_dir_identity) != 0 ||
        !S_ISDIR(retained_dir_identity.st_mode)) {
        errno = errno == 0 ? EPROTO : errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("fstat-retained-dir-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_RETAINED_DIR_FSTAT_OK");

    struct stat retained_stat_identity;
    if (fstat(open_stat, &retained_stat_identity) != 0 ||
        !S_ISREG(retained_stat_identity.st_mode)) {
        errno = errno == 0 ? EPROTO : errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("fstat-retained-stat-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_RETAINED_STAT_FSTAT_OK");

    struct stat empty_path_identity;
    errno = 0;
    if (fstatat(proc_dirfd, "", &empty_path_identity, AT_EMPTY_PATH) != 0 ||
        !S_ISDIR(empty_path_identity.st_mode)) {
        errno = errno == 0 ? EPROTO : errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("fstatat-retained-dir-empty-path-after-reap");
    }
    errno = 0;
    if (fstatat(open_stat, "", &empty_path_identity, AT_EMPTY_PATH) != 0 ||
        !S_ISREG(empty_path_identity.st_mode)) {
        errno = errno == 0 ? EPROTO : errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("fstatat-retained-stat-empty-path-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_RETAINED_EMPTY_PATH_FSTATAT_OK");

    errno = 0;
    int dot_open = openat(proc_dirfd, ".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    int dot_open_errno = errno;
    if (dot_open >= 0 || dot_open_errno != ESRCH) {
        int failure_errno = dot_open_errno == 0 ? EPROTO : dot_open_errno;
        if (dot_open >= 0)
            close(dot_open);
        close(open_stat);
        close(proc_dirfd);
        errno = failure_errno;
        return fail("dir-dot-open-after-reap");
    }

    errno = 0;
    if (fstatat(proc_dirfd, ".", &empty_path_identity, 0) != -1 ||
        errno != ESRCH) {
        int failure_errno = errno == 0 ? EPROTO : errno;
        close(open_stat);
        close(proc_dirfd);
        errno = failure_errno;
        return fail("dir-dot-stat-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_DIRFD_DOT_SEARCH_ESRCH_OK");

    char dirents[4096];
    errno = 0;
    long dirents_result = syscall(SYS_getdents64, proc_dirfd, dirents,
                                  sizeof(dirents));
    int dirents_errno = errno;
    if (dirents_result != -1 || dirents_errno != ENOENT) {
        int failure_errno = dirents_errno == 0 ? EPROTO : dirents_errno;
        close(open_stat);
        close(proc_dirfd);
        errno = failure_errno;
        return fail("dir-getdents-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_DIRFD_GETDENTS_ENOENT_OK");

    errno = 0;
    int absent_stat = proc_stat_open(child);
    int absent_stat_errno = errno;
    if (absent_stat >= 0 || absent_stat_errno != ENOENT) {
        int failure_errno = absent_stat_errno == 0 ? EPROTO : absent_stat_errno;
        if (absent_stat >= 0)
            close(absent_stat);
        close(open_stat);
        close(proc_dirfd);
        errno = failure_errno;
        return fail("absolute-stat-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_ABSOLUTE_LOOKUP_ENOENT_OK");

    errno = 0;
    int dir_open_read = proc_stat_openat(proc_dirfd);
    int dir_open_errno = errno;
    if (dir_open_read != -1 || dir_open_errno != ESRCH) {
        int failure_errno = dir_open_errno == 0 ? EPROTO : dir_open_errno;
        if (dir_open_read >= 0)
            close(dir_open_read);
        close(open_stat);
        close(proc_dirfd);
        errno = failure_errno;
        return fail("dir-stat-after-reap");
    }
    puts("THEKERNEL_PROC_ZOMBIE_DIRFD_OPENAT_AFTER_REAP_ESRCH_OK");

    /* A retained proc directory must not become an alias for a later process
     * that reuses the same numeric PID.  PID reuse is normally too expensive
     * to force, so this is deliberately a bounded probe rather than a loop
     * that waits indefinitely for wraparound. */
    int pid_reused = 0;
    pid_t replacement = -1;
    for (int attempt = 0; attempt < PID_REUSE_ATTEMPTS; attempt++) {
        replacement = fork();
        if (replacement < 0) {
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-fork");
        }
        if (replacement == 0)
            _exit(43);
        if (replacement != child) {
            if (waitpid(replacement, &status, 0) != replacement) {
                close(open_stat);
                close(proc_dirfd);
                return fail("pid-identity-waitpid");
            }
            if (!WIFEXITED(status) || WEXITSTATUS(status) != 43) {
                errno = EPROTO;
                close(open_stat);
                close(proc_dirfd);
                return fail("pid-identity-wait-status");
            }
            continue;
        }

        pid_reused = 1;
        if (wait_for_zombie(replacement) != 0) {
            close(open_stat);
            close(proc_dirfd);
            return 1;
        }

        int replacement_dirfd = proc_dir_open(replacement);
        if (replacement_dirfd < 0) {
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-open-dir");
        }
        struct stat replacement_dir_identity;
        if (fstat(replacement_dirfd, &replacement_dir_identity) != 0) {
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-stat-dir");
        }
        state = '\0';
        unsigned long long replacement_start_time = 0;
        if (proc_stat_identity(replacement, &state, &replacement_start_time) !=
                1 ||
            state != 'Z') {
            errno = EPROTO;
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-stat");
        }
        int same_inode = old_dir_identity.st_dev == replacement_dir_identity.st_dev &&
                         old_dir_identity.st_ino == replacement_dir_identity.st_ino;
        int same_start_time = old_start_time == replacement_start_time;
        if (same_inode && same_start_time) {
            errno = EPROTO;
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-alias");
        }

        errno = 0;
        dir_open_read = proc_stat_openat(proc_dirfd);
        dir_open_errno = errno;
        if (dir_open_read != -1 || dir_open_errno != ESRCH) {
            int failure_errno = dir_open_errno == 0 ? EPROTO : dir_open_errno;
            if (dir_open_read >= 0)
                close(dir_open_read);
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            errno = failure_errno;
            return fail("pid-identity-stale-dirfd");
        }
        if (waitpid(replacement, &status, 0) != replacement) {
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-waitpid");
        }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 43) {
            errno = EPROTO;
            close(replacement_dirfd);
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-wait-status");
        }
        if (close(replacement_dirfd) != 0) {
            close(open_stat);
            close(proc_dirfd);
            return fail("pid-identity-close-dir");
        }
        break;
    }

    errno = 0;
    open_read = proc_stat_read(open_stat, &state);
    open_read_errno = errno;
    if (open_read != -1 || open_read_errno != ESRCH) {
        errno = open_read_errno == 0 ? EPROTO : open_read_errno;
        close(open_stat);
        close(proc_dirfd);
        return fail("pid-identity-stale-handle");
    }
    if (close(open_stat) != 0) {
        close(proc_dirfd);
        return fail("pid-identity-close");
    }
    if (close(proc_dirfd) != 0)
        return fail("pid-identity-close-dir");
    if (replacement < 0) {
        errno = EPROTO;
        return fail("pid-identity-missing-replacement");
    }
    if (pid_reused)
        puts("THEKERNEL_PROC_ZOMBIE_PID_IDENTITY_REUSED_OK");
    else
        puts("THEKERNEL_PROC_ZOMBIE_PID_IDENTITY_UNSUPPORTED");
    puts("THEKERNEL_PROC_ZOMBIE_PID_IDENTITY_CHECKED");

    if (wait_for_reap_visibility_to_clear(child) != 0)
        return 1;
    if (proc_root_contains(replacement) != 0) {
        errno = EPROTO;
        return fail("replacement-reap-visibility");
    }
    puts("THEKERNEL_PROC_ZOMBIE_ROOT_HIDDEN_OK");
    puts("THEKERNEL_PROC_ZOMBIE_STAT_HIDDEN_OK");
    puts("THEKERNEL_PROC_ZOMBIE_OK");
    return 0;
}
