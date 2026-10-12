/* Best-effort userspace kernel-log UDP relay, not a panic/IRQ-time netconsole. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/klog.h>
#include <sys/socket.h>
#include <unistd.h>

int main(int argc, char **argv) {
    int from_stdin = argc == 4 && !strcmp(argv[3], "--stdin");
    if (argc != 3 && !from_stdin) { fprintf(stderr, "usage: thekernel-netconsole HOST_IP PORT [--stdin]\n"); return 2; }
    char *end;
    long port = strtol(argv[2], &end, 10);
    struct sockaddr_in destination = { .sin_family = AF_INET };
    if (*end || port < 1 || port > 65535 || inet_pton(AF_INET, argv[1], &destination.sin_addr) != 1) return 2;
    destination.sin_port = htons((unsigned short)port);
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) { perror("netconsole socket"); return 1; }
    char buffer[1200];
    if (from_stdin) {
        /* Explicit userspace probe output; never write it into kernel logs. */
        int failed = 0;
        while (fgets(buffer, sizeof buffer, stdin)) {
            size_t length = strlen(buffer);
            if (sendto(fd, buffer, length, 0, (struct sockaddr *)&destination, sizeof destination) != (ssize_t)length)
                failed = 1;
        }
        failed |= ferror(stdin) != 0;
        close(fd);
        return failed;
    }
    puts("N305_NETCONSOLE_READY userspace relay; early boot/panic delivery not guaranteed");
    fflush(stdout);
    int warned = 0;
    for (;;) {
        /* SYSLOG_ACTION_READ has its own cursor, not the screen's cursor. */
        int bytes = klogctl(2, buffer, sizeof buffer);
        if (bytes < 0) { if (errno == EINTR) continue; perror("netconsole syslog"); break; }
        if (sendto(fd, buffer, (size_t)bytes, 0, (struct sockaddr *)&destination, sizeof destination) < 0 && !warned) {
            perror("netconsole UDP dropped (screen remains available)"); warned = 1;
        }
    }
    close(fd);
    return 1;
}
