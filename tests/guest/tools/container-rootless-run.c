#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <unistd.h>

/* Explicit guest-only identity transition. No host user/namespace setup. */
int main(int argc, char **argv) {
    if (argc < 3) return 2;
    int fd = open(argv[1], O_WRONLY | O_CLOEXEC);
    char pid[32]; int count = snprintf(pid, sizeof(pid), "%ld", (long)getpid());
    if (fd < 0) { perror("open delegated cgroup"); return 1; }
    if (write(fd, pid, (size_t)count) != count) { perror("join delegated cgroup"); return 1; }
    if (close(fd)) return 1;
    fd = open(argv[1], O_RDONLY | O_CLOEXEC);
    char members[64] = {0};
    ssize_t got = fd < 0 ? -1 : read(fd, members, sizeof(members)-1);
    if (got < 1 || strtol(members, NULL, 10) != getpid()) { perror("read caller-visible cgroup member"); return 1; }
    close(fd);
    if (setgroups(0, NULL) || setgid(1000) || setuid(1000)) { perror("drop rootless IDs"); return 1; }
    if (getuid() != 1000 || geteuid() != 1000 || getgid() != 1000 || getegid() != 1000) return 1;
    puts("THEKERNEL_ROOTLESS_UID=1000"); fflush(stdout);
    execvp(argv[2], argv + 2); perror("exec rootless tool"); return 1;
}
