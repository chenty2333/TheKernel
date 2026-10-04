#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

static int read_number(const char *path, unsigned long long *value) {
    FILE *file = fopen(path, "r");
    if (!file) return -1;
    int result = fscanf(file, "%llu", value) == 1 ? 0 : -1;
    fclose(file);
    return result;
}
int main(void) {
    unsigned long long sectors, ro, removable, logical;
    if (read_number("/sys/class/block/vda/size", &sectors) ||
        read_number("/sys/block/vda/ro", &ro) ||
        read_number("/sys/block/vda/removable", &removable) ||
        read_number("/sys/block/vda/queue/logical_block_size", &logical)) return 1;
    char dev[64], resolved[256];
    FILE *file = fopen("/sys/block/vda/dev", "r");
    if (!file || !fgets(dev, sizeof(dev), file)) return 2;
    fclose(file);
    dev[strcspn(dev, "\n")] = 0;
    snprintf(resolved, sizeof(resolved), "/sys/dev/block/%s/size", dev);
    unsigned long long linked;
    if (read_number(resolved, &linked) || linked != sectors) return 3;
    int fd = open("/dev/vda", O_RDONLY);
    unsigned long long bytes = 0;
    int block_size = 0, read_only = -1;
    if (fd < 0 || ioctl(fd, BLKGETSIZE64, &bytes) || ioctl(fd, BLKSSZGET, &block_size) ||
        ioctl(fd, BLKROGET, &read_only)) return 4;
    close(fd);
    if (bytes / 512 != sectors || logical != (unsigned) block_size ||
        ro != (unsigned) read_only || removable != 0) return 5;
    file = fopen("/proc/partitions", "r");
    if (!file) return 6;
    char line[256], name[128];
    unsigned major, minor;
    unsigned long long kib;
    int found = 0;
    while (fgets(line, sizeof(line), file)) {
        if (sscanf(line, "%u %u %llu %127s", &major, &minor, &kib, name) == 4 &&
            !strcmp(name, "vda")) {
            char rowdev[64];
            snprintf(rowdev, sizeof(rowdev), "%u:%u", major, minor);
            if (strcmp(rowdev, dev) || kib != sectors / 2) return 7;
            found = 1;
        }
    }
    fclose(file);
    if (!found) return 8;
    puts("THEKERNEL_BLOCK_INVENTORY_OK");
    return 0;
}
