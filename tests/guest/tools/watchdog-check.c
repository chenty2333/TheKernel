#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/watchdog.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int fd = open("/dev/watchdog", O_WRONLY);
    if (fd < 0) { perror("watchdog open"); return 1; }
    struct watchdog_info info;
    int timeout = 6, left = -1, status = -1;
    if (ioctl(fd, WDIOC_GETSUPPORT, &info) || ioctl(fd, WDIOC_SETTIMEOUT, &timeout) ||
        ioctl(fd, WDIOC_GETTIMEOUT, &timeout) || ioctl(fd, WDIOC_GETSTATUS, &status) ||
        ioctl(fd, WDIOC_GETTIMELEFT, &left)) return 1;
    printf("ITCO_ABI version=%u timeout=%d left=%d options=%x status=%d\n",info.firmware_version, timeout,left,info.options,status);
    if (timeout != 6 || !(info.options & WDIOF_MAGICCLOSE) || left < 0) return 1;
    if (!strcmp(argv[1], "expire")) {
        puts("ITCO_STOP_FEEDING"); fflush(stdout);
        sleep(30);
        puts("ITCO_FAILED_TO_RESET"); return 1;
    }
    if (strcmp(argv[1], "feed")) return 2;
    for (int i=0; i<20; ++i) {
        if (i%2 ? ioctl(fd,WDIOC_KEEPALIVE,0) : write(fd,"K",1)!=1) return 1;
        sleep(1);
    }
    if (write(fd,"V",1)!=1 || close(fd)) return 1;
    sleep(14); /* longer than QEMU ICH9 two-stage timeout, stopped via magic close */
    puts("ITCO_FEED_AND_MAGIC_CLOSE_OK"); fflush(stdout);
    return 0;
}
