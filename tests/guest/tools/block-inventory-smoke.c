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
static int verify_gpt(void) {
    static const struct { const char *path; unsigned long long expected; } rows[] = {
        { "/sys/block/nvme0n1/size", 32768 },
        { "/sys/block/nvme0n1/nvme0n1p1/size", 8192 },
        { "/sys/block/nvme0n1/nvme0n1p1/start", 2048 },
        { "/sys/class/block/nvme0n1p1/partition", 1 },
        { "/sys/class/block/nvme0n1p1/ro", 1 },
    };
    for (unsigned i = 0; i < sizeof(rows) / sizeof(rows[0]); i++) {
        unsigned long long value;
        if (read_number(rows[i].path, &value) || value != rows[i].expected) {
            fprintf(stderr, "GPT inventory mismatch: %s\n", rows[i].path);
            return 10;
        }
    }
    FILE *file = fopen("/sys/class/block/nvme0n1p1/dev", "r");
    char dev[64], path[256];
    if (!file || !fgets(dev, sizeof(dev), file)) return 11;
    fclose(file);
    dev[strcspn(dev, "\n")] = 0;
    snprintf(path, sizeof(path), "/sys/dev/block/%s/start", dev);
    unsigned long long start;
    if (read_number(path, &start) || start != 2048) return 12;
    // RO is a real fail-closed device policy, not just a sysfs bit.
    int fd = open("/dev/nvme0n1p1", O_RDWR);
    if (fd < 0) return 13;
    unsigned char before[512], after[512], attempted[512];
    if (pread(fd, before, sizeof(before), 0) != sizeof(before)) return 14;
    memcpy(attempted, before, sizeof(attempted));
    attempted[0] ^= 0x5a;
    errno = 0;
    ssize_t written = pwrite(fd, attempted, sizeof(attempted), 0);
    int write_errno = errno;
    if (written != -1 || (write_errno != EROFS && write_errno != EPERM)) return 15;
    if (pread(fd, after, sizeof(after), 0) != sizeof(after) ||
        memcmp(before, after, sizeof(before))) return 16;
    close(fd);
    puts("THEKERNEL_BLOCK_GPT_INVENTORY_OK");
    return 0;
}
int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "--gpt")) return verify_gpt();
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
