#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <sys/wait.h>

struct values { unsigned long long faults, major, free, anon, cached, tables; };
static int snapshot(struct values *v) {
    FILE *f = fopen("/proc/vmstat", "r");
    if (!f) { perror("vmstat"); return 1; }
    char line[256], name[128], extra;
    unsigned long long n;
    unsigned int found = 0;
    memset(v, 0, sizeof(*v));
    while (fgets(line, sizeof(line), f)) {
        if (sscanf(line, "%127s %llu %c", name, &n, &extra) != 2) return 1;
#define FIELD(key, member, mask) if (!strcmp(name, key)) { v->member = n; found |= mask; }
        FIELD("pgfault", faults, 1)
        FIELD("pgmajfault", major, 2)
        FIELD("nr_free_pages", free, 4)
        FIELD("nr_anon_pages", anon, 8)
        FIELD("nr_file_pages", cached, 16)
        FIELD("nr_page_table_pages", tables, 32)
#undef FIELD
    }
    int error = ferror(f);
    if (fclose(f) || error || found != 63 || v->major > v->faults || !v->free) return 1;
    return 0;
}
int main(void) {
    long page = sysconf(_SC_PAGESIZE);
    if (page <= 0) return 1;
    size_t bytes = (size_t)page*32;
    struct values before, after;
    volatile unsigned char *memory = mmap(NULL, bytes, PROT_READ|PROT_WRITE,
                                        MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED) return 1;
    for (unsigned int i = 0; i < 32; i++) memory[(size_t)i*page] = (unsigned char)i;
    /* mmap population is not universally lazy. Force a real private COW
     * write after fork instead of assuming each initial store must fault. */
    if (snapshot(&before)) return 1;
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        for (unsigned int i = 0; i < 32; i++) memory[(size_t)i*page] = (unsigned char)(i+1);
        _exit(0);
    }
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 1;
    if (snapshot(&after)) return 1;
    if (after.faults <= before.faults) {
        fprintf(stderr, "vmstat did not reflect real COW fault activity before=%llu after=%llu\n", before.faults, after.faults); return 1;
    }
    if (munmap((void *)memory, bytes)) return 1;
    printf("PROC_VMSTAT_OK faults_delta=%llu anon_before=%llu anon_after=%llu\n",
           after.faults-before.faults, before.anon, after.anon);
    return 0;
}
