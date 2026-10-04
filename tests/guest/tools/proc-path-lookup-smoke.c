#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    int dir = open("/proc/self", O_DIRECTORY | O_RDONLY | O_CLOEXEC);
    if (dir < 0) { perror("proc-self"); return 1; }
    for (unsigned int i = 0; i < 2; i++) {
        const char *leaf = i ? "mounts" : "mountinfo";
        char absolute[64];
        snprintf(absolute, sizeof(absolute), "/proc/self/%s", leaf);
        struct stat st;
        if (stat(absolute, &st) || !S_ISREG(st.st_mode)) return 1;
        for (unsigned int relative = 0; relative < 2; relative++) {
            int fd = openat(relative ? dir : AT_FDCWD, relative ? leaf : absolute,
                            O_RDONLY | O_CLOEXEC);
            if (fd < 0) { perror(absolute); return 1; }
            char buffer[8192];
            ssize_t bytes = read(fd, buffer, sizeof(buffer)-1);
            if (close(fd) || bytes <= 0) return 1;
            buffer[bytes] = 0;
            if (!strchr(buffer, '\n') || (i == 0 && !strstr(buffer, " - "))) return 1;
        }
    }
    for (unsigned int i = 0; i < 2; i++) {
        char buffer[4096];
        ssize_t bytes = readlinkat(dir, i ? "root" : "cwd", buffer, sizeof(buffer));
        if (bytes <= 0 || buffer[0] != '/') return 1;
    }
    if (close(dir)) return 1;
    puts("PROC_PATH_LOOKUP_OK");
    return 0;
}
