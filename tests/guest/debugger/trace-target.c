#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#define FILE_PATH "/tmp/thekernel-debugger-data.txt"
int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--trace-child") == 0) {
        int fd = (int)syscall(SYS_openat, AT_FDCWD, FILE_PATH, O_RDWR | O_CREAT | O_TRUNC, 0600);
        if (fd < 0 || syscall(SYS_write, fd, "trace-data", 10) != 10 || syscall(SYS_lseek, fd, 0, SEEK_SET) != 0) return 2;
        char data[10];
        if (syscall(SYS_read, fd, data, sizeof(data)) != 10 || memcmp(data, "trace-data", 10) != 0) return 3;
        if (syscall(SYS_close, fd) != 0 || syscall(SYS_unlink, FILE_PATH) != 0) return 4;
        return 0;
    }
    pid_t pid = (pid_t)syscall(SYS_fork);
    if (pid < 0) return 5;
    if (!pid) {
        char *args[] = {argv[0], "--trace-child", NULL}; char *env[] = {NULL};
        syscall(SYS_execve, argv[0], args, env); _exit(6);
    }
    int status;
    if (waitpid(pid, &status, 0) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 7;
    puts("TRACE_TARGET_OK"); return 0;
}
