#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int fail(const char *stage) {
    fprintf(stderr, "THEKERNEL_UMASK_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static mode_t read_umask(void) {
    mode_t value = (mode_t)syscall(SYS_umask, 0);
    (void)syscall(SYS_umask, value);
    return value;
}

static int expect_umask(mode_t expected, const char *stage) {
    if (read_umask() != expected) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int wait_success(pid_t child, const char *stage) {
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) ||
        WEXITSTATUS(status) != 0) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int verify_create(mode_t expected) {
    char path[128];
    if (snprintf(path, sizeof(path), "/tmp/thekernel-umask-%ld", (long)getpid()) >=
        (int)sizeof(path)) {
        errno = ENAMETOOLONG;
        return fail("create-path");
    }
    (void)unlink(path);
    int fd = open(path, O_CREAT | O_EXCL | O_RDWR, 0777);
    struct stat st;
    if (fd < 0 || fstat(fd, &st) != 0 || (st.st_mode & 0777) != (0777 & ~expected)) {
        if (fd >= 0) (void)close(fd);
        (void)unlink(path);
        errno = EPROTO;
        return fail("create-mode");
    }
    if (close(fd) != 0 || unlink(path) != 0) return fail("create-cleanup");
    return 0;
}

static int exec_stage(void) {
    if (expect_umask(0062, "exec-preserves-fs")) return 1;
    (void)syscall(SYS_umask, 0);
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--exec-stage") == 0) return exec_stage();

    puts("THEKERNEL_ABI_CASE umask.raw-differential");
    mode_t initial = (mode_t)syscall(SYS_umask, 0xfffff9abU);
    if (expect_umask(0653, "mask-low-32-0777")) return 1;
    if (syscall(SYS_umask, 0027) != 0653 || verify_create(0027)) return 1;
    puts("THEKERNEL_ABI_ASSERT umask.raw-differential MASK_AND_CREATE pass");

    pid_t child = fork();
    if (child < 0) return fail("fork");
    if (child == 0) {
        (void)syscall(SYS_umask, 0077);
        _exit(expect_umask(0077, "fork-child"));
    }
    if (wait_success(child, "fork-wait") || expect_umask(0027, "fork-copies-fs")) return 1;
    puts("THEKERNEL_ABI_ASSERT umask.raw-differential FORK_COPIES_FS pass");

    child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_FS | SIGCHLD), 0, 0, 0, 0);
    if (child < 0) return fail("clone-fs");
    if (child == 0) {
        (void)syscall(SYS_umask, 0077);
        _exit(expect_umask(0077, "clone-child"));
    }
    if (wait_success(child, "clone-fs-wait") || expect_umask(0077, "clone-fs-shares")) return 1;
    puts("THEKERNEL_ABI_ASSERT umask.raw-differential CLONE_FS_SHARES pass");

    child = (pid_t)syscall(SYS_clone, (unsigned long)(CLONE_FS | SIGCHLD), 0, 0, 0, 0);
    if (child < 0) return fail("clone-unshare");
    if (child == 0) {
        if (syscall(SYS_unshare, CLONE_FS) != 0) _exit(1);
        (void)syscall(SYS_umask, 0002);
        _exit(expect_umask(0002, "unshare-child"));
    }
    if (wait_success(child, "unshare-wait") || expect_umask(0077, "unshare-separates")) return 1;
    puts("THEKERNEL_ABI_ASSERT umask.raw-differential UNSHARE_FS_SEPARATES pass");

    child = fork();
    if (child < 0) return fail("exec-fork");
    if (child == 0) {
        (void)syscall(SYS_umask, 0062);
        execl("/proc/self/exe", "/proc/self/exe", "--exec-stage", (char *)NULL);
        _exit(1);
    }
    if (wait_success(child, "exec-wait")) return 1;
    (void)syscall(SYS_umask, initial);
    puts("THEKERNEL_ABI_ASSERT umask.raw-differential EXEC_PRESERVES_FS pass");
    puts("THEKERNEL_ABI_RESULT umask.raw-differential pass");
    return 0;
}
