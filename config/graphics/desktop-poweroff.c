#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

/* This desktop-only setuid helper permits exactly one operation: ask BusyBox
 * init to run its normal shutdown sequence, including filesystem unmounts.
 * It never accepts a command, path, or option from the caller. */
int main(int argc, char **argv)
{
    (void)argv;
    if (argc != 1) {
        fputs("desktop-poweroff takes no arguments\n", stderr);
        return 1;
    }
    if (clearenv() || setenv("PATH", "/sbin:/bin:/usr/sbin:/usr/bin", 1) ||
        setgid(0) || setuid(0)) {
        perror("desktop-poweroff");
        return 1;
    }
    execl("/sbin/poweroff", "poweroff", (char *)NULL);
    perror("desktop-poweroff");
    return 1;
}
