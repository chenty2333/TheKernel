#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static volatile int shared_value;
static const struct timespec child_delay = {
    .tv_sec = 0,
    .tv_nsec = 20 * 1000 * 1000,
};
static pid_t vfork_child;

static int fail(const char *stage)
{
    fprintf(stderr, "THEKERNEL_VFORK_FAIL %s errno=%d (%s)\n", stage, errno,
            strerror(errno));
    return 1;
}

static void child_exit(int status)
{
    syscall(SYS_exit, status);
    for (;;) {
    }
}

static int wait_success(pid_t child, const char *stage)
{
    int status = 0;
    if (syscall(SYS_wait4, child, &status, 0, NULL) != child)
        return fail(stage);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = EPROTO;
        return fail(stage);
    }
    return 0;
}

static int test_exit_release(void)
{
    pid_t child;

    shared_value = 0;
    child = vfork();
    if (child < 0)
        return fail("exit-vfork");
    if (child == 0) {
        shared_value = 1;
        syscall(SYS_nanosleep, &child_delay, NULL);
        shared_value = 2;
        child_exit(0);
    }

    vfork_child = child;
    if (shared_value != 2) {
        errno = EPROTO;
        return fail("exit-parent-resumed-before-child-release");
    }
    if (wait_success(vfork_child, "exit-wait") != 0)
        return 1;
    puts("THEKERNEL_VFORK_EXIT_OK");
    return 0;
}

static int test_exec_release(void)
{
    pid_t child;

    shared_value = 0;
    child = vfork();
    if (child < 0)
        return fail("exec-vfork");
    if (child == 0) {
        shared_value = 3;
        execl("/bin/true", "true", (char *)NULL);
        child_exit(127);
    }

    vfork_child = child;
    if (shared_value != 3) {
        errno = EPROTO;
        return fail("exec-parent-resumed-before-child-exec");
    }
    if (wait_success(vfork_child, "exec-wait") != 0)
        return 1;
    puts("THEKERNEL_VFORK_EXEC_OK");
    return 0;
}

int main(void)
{
    if (test_exit_release() != 0 || test_exec_release() != 0)
        return 1;
    puts("THEKERNEL_VFORK_OK");
    return 0;
}
