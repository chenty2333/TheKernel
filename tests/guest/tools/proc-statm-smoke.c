#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static int read_statm(const char *path, unsigned long long fields[7]) {
    FILE *file = fopen(path, "r");
    if (!file) { perror(path); return 1; }
    char extra;
    int count = fscanf(file, "%llu %llu %llu %llu %llu %llu %llu %c",
                       &fields[0], &fields[1], &fields[2], &fields[3],
                       &fields[4], &fields[5], &fields[6], &extra);
    if (fclose(file) || count != 7 || fields[0] < fields[1] || fields[1] < fields[2] ||
        fields[3] > fields[0] || fields[5] > fields[0] || fields[4] || fields[6] || !fields[1]) {
        fprintf(stderr, "STATM_INVALID path=%s count=%d values=%llu %llu %llu %llu %llu %llu %llu\n",
                path, count, fields[0],fields[1],fields[2],fields[3],fields[4],fields[5],fields[6]);
        return 1;
    }
    return 0;
}
int main(void) {
    unsigned long long before[7], after[7];
    if (read_statm("/proc/self/statm", before)) return 1;
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0) return 1;
    size_t bytes = (size_t)page*32;
    volatile unsigned char *memory = mmap(NULL, bytes, PROT_READ|PROT_WRITE,
                                        MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED) return 1;
    for (unsigned int i = 0; i < 32; i++) memory[(size_t)i*page] = (unsigned char)i;
    if (read_statm("/proc/self/statm", after)) return 1;
    if (after[0] < before[0]+32 || after[1] < before[1]) {
        fprintf(stderr, "STATM_GROWTH before=%llu/%llu after=%llu/%llu\n", before[0],before[1],after[0],after[1]); return 1;
    }
    char parent[64];
    snprintf(parent, sizeof(parent), "/proc/%ld/statm", (long)getpid());
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        if (geteuid() == 0 && setuid(1000)) _exit(1);
        unsigned long long other[7];
        _exit(read_statm(parent, other));
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 1;
    if (munmap((void *)memory, bytes)) return 1;
    printf("PROC_STATM_OK size=%llu resident=%llu shared=%llu\n", after[0], after[1], after[2]);
    return 0;
}
