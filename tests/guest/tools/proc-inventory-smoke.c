#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

int main(void) {
    FILE *file = fopen("/proc/filesystems", "r");
    if (!file) { perror("filesystems"); return 1; }
    char row[256];
    unsigned int proc = 0, tmpfs = 0, ext4 = 0, count = 0;
    while (fgets(row, sizeof(row), file)) {
        char *tab = strchr(row, '\t');
        char *newline = strchr(row, '\n');
        if (!tab || !newline || newline[1] || tab >= newline) return 1;
        *tab = '\0'; *newline = '\0';
        if (row[0] && strcmp(row, "nodev")) return 1;
        if (strchr(tab + 1, '\t') || strchr(tab + 1, ' ') || !tab[1]) return 1;
        if (!strcmp(tab + 1, "proc")) proc += !strcmp(row, "nodev");
        if (!strcmp(tab + 1, "tmpfs")) tmpfs += !strcmp(row, "nodev");
        if (!strcmp(tab + 1, "ext4")) ext4 += row[0] == '\0';
        count++;
    }
    int error = ferror(file);
    if (fclose(file) || error || proc != 1 || tmpfs != 1 || ext4 != 1) return 1;
    int fd = open("/proc/modules", O_RDONLY | O_CLOEXEC);
    if (fd < 0) { perror("modules"); return 1; }
    /* TheKernel has no dynamically loaded modules. Do not invent entries for
     * built-in drivers, which Linux does not list here either. */
    ssize_t bytes = read(fd, row, sizeof(row));
    if (close(fd) || bytes != 0) return 1;
    struct stat st;
    if (stat("/proc/filesystems", &st) || (st.st_mode & 0222)) return 1;
    printf("PROC_INVENTORY_OK filesystems=%u loaded_modules=0\n", count);
    return 0;
}
