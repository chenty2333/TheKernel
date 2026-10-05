#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TARGET "/opt/thekernel-tests/debugger/debug-target"
#define SCRIPT "/opt/thekernel-tests/debugger/basic.gdb"
#define TRANSCRIPT "/tmp/thekernel-debugger-output.txt"
static char output[131072];
static long long millis(void) {
    struct timespec now; if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (long long)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}
static int run_tool(char *const args[], const char *name) {
    int fd = open(TRANSCRIPT, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return 1;
    pid_t pid = fork(); if (pid < 0) { close(fd); return 1; }
    if (!pid) {
        dup2(fd, STDOUT_FILENO); dup2(fd, STDERR_FILENO); close(fd);
        setenv("DEBUGINFOD_URLS", "", 1);
        execv(args[0], args); _exit(127);
    }
    close(fd); int status = 0; long long deadline = millis() + 90000;
    for (;;) {
        pid_t result = waitpid(pid, &status, WNOHANG);
        if (result == pid) break;
        if (result < 0 || millis() >= deadline) {
            kill(pid, SIGKILL); waitpid(pid, &status, 0);
            fprintf(stderr, "debugger-smoke: %s timeout/wait failure\n", name);
            status = 1 << 8; break;
        }
        usleep(10000);
    }
    fd = open(TRANSCRIPT, O_RDONLY); if (fd < 0) return 1;
    ssize_t count = read(fd, output, sizeof(output) - 1); close(fd);
    if (count < 0) return 1;
    output[count] = 0;
    printf("DEBUGGER_TOOL_BEGIN %s\n%s\nDEBUGGER_TOOL_END %s\n", name, output, name);
    return !WIFEXITED(status) || WEXITSTATUS(status) != 0;
}
/* strace aligns return columns; compare tokens, not variable padding. */
static void normalize_spacing(void) {
    char *read = output, *write = output;
    int pending_space = 0;
    while (*read) {
        if (*read == ' ' || *read == '\t' || *read == '\r' || *read == '\n') {
            pending_space = write != output;
        } else {
            if (pending_space) *write++ = ' ';
            *write++ = *read;
            pending_space = 0;
        }
        read++;
    }
    *write = 0;
}
int main(int argc, char **argv) {
    int follow = !(argc == 2 && strcmp(argv[1], "--files-only") == 0);
    char *gdb_version[] = {"/usr/bin/gdb", "-nx", "--version", NULL};
    char *strace_version[] = {"/usr/bin/strace", "--version", NULL};
    if (run_tool(gdb_version, "gdb-version") || !strstr(output, "GNU gdb (GDB) 16.3")) return 1;
    if (run_tool(strace_version, "strace-version") || !strstr(output, "strace -- version 6.19")) return 1;
    char *gdb[] = {"/usr/bin/gdb", "-q", "-nx", "-batch", "-x", SCRIPT, "--args", TARGET, NULL};
    if (run_tool(gdb, "gdb-basic")) return 1;
    if (!strstr(output, "Breakpoint 1") || !strstr(output, "middle (input=4)") ||
        !strstr(output, "leaf (input=4)") || !strstr(output, "rip") ||
        !strstr(output, "= 12") || !strstr(output, "DEBUG_RESULT=24") ||
        !strstr(output, "exited normally")) return 1;
    puts("THEKERNEL_REAL_GDB_BASIC_OK");
    char *trace[] = {"/usr/bin/strace", "-f", "-qq", "-s", "64", "-e",
        "trace=fork,clone,vfork,execve,openat,write,lseek,read,close,unlink,wait4",
        "/opt/thekernel-tests/debugger/trace-target", follow ? NULL : "--trace-child", NULL};
    if (run_tool(trace, follow ? "strace-fork-exec-file" : "strace-file")) return 1;
    normalize_spacing();
    if (follow && (!strstr(output, "fork(") || !strstr(output, "execve(") ||
                   !strstr(output, "TRACE_TARGET_OK"))) return 1;
    const char *calls[] = {
        "openat(AT_FDCWD, \"/tmp/thekernel-debugger-data.txt\", O_RDWR|O_CREAT|O_TRUNC, 0600) = 3",
        "write(3, \"trace-data\", 10) = 10", "lseek(3, 0, SEEK_SET) = 0",
        "read(3, \"trace-data\", 10) = 10", "close(3) = 0",
        "unlink(\"/tmp/thekernel-debugger-data.txt\") = 0"
    };
    const char *cursor = output;
    for (size_t index = 0; index < sizeof(calls) / sizeof(calls[0]); ++index) {
        const char *found = strstr(cursor, calls[index]);
        if (!found) return 1;
        cursor = found + strlen(calls[index]);
    }
    puts(follow ? "THEKERNEL_REAL_STRACE_FORK_EXEC_FILE_OK" : "THEKERNEL_REAL_STRACE_FILE_OK");
    return 0;
}
