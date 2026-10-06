#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/mman.h>
#include <stdint.h>
#include <unistd.h>

/* PID1 is the sole initial member. Children remain alive until the failed
 * fork is observed; success cannot be confused with children exiting early. */
static int pids_probe(void) {
    int hold[2]; pid_t children[8]; size_t count = 0;
    if (pipe(hold) != 0) return 1;
    while (count < 8) {
        errno = 0; pid_t child = fork();
        if (child == -1) break;
        if (!child) {
            close(hold[1]); char byte;
            while (read(hold[0], &byte, 1) == -1 && errno == EINTR) {}
            close(hold[0]); _exit(0);
        }
        children[count++] = child;
    }
    int limited = count == 7 && errno == EAGAIN;
    close(hold[0]); close(hold[1]);
    for (size_t i = 0; i < count; ++i) {
        int status;
        if (waitpid(children[i], &status, 0) != children[i] ||
            !WIFEXITED(status) || WEXITSTATUS(status)) limited = 0;
    }
    if (!limited) { fprintf(stderr, "pids limit mismatch: children=%zu\n", count); return 1; }
    puts("CRUN_PIDS_FORK_EAGAIN_OK children=7 limit=8");
    return 0;
}
static uint64_t memory_current(void) {
    FILE *file = fopen("/cg/memory.current", "r"); unsigned long long value;
    if (!file || fscanf(file, "%llu", &value) != 1) exit(3);
    fclose(file); return (uint64_t)value;
}
static int memory_probe(int oom) {
    if (oom) {
        size_t size = 256UL * 1024 * 1024;
        volatile unsigned char *pages = mmap(NULL, size, PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (pages == MAP_FAILED) return 4;
        puts("CRUN_MEMORY_TOUCH_BEGIN"); fflush(stdout);
        for (size_t offset = 0; offset < size; offset += 4096) pages[offset] = 0x5a;
        puts("CRUN_MEMORY_LIMIT_FAILED_TO_KILL"); return 5;
    }
    /* Sparse virtual address reservation must not consume a resident budget. */
    void *sparse = mmap(NULL, 512UL * 1024 * 1024, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (sparse == MAP_FAILED || munmap(sparse, 512UL * 1024 * 1024)) return 6;
    uint64_t before = memory_current(); size_t size = 4UL * 1024 * 1024;
    volatile unsigned char *pages = mmap(NULL, size, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (pages == MAP_FAILED) return 7;
    for (size_t offset = 0; offset < size; offset += 4096) pages[offset] = 0x6b;
    uint64_t resident = memory_current();
    if (resident < before + size || munmap((void *)pages, size)) return 8;
    uint64_t after = memory_current();
    /* The probe's own newly faulted libc/code pages may remain charged. */
    if (after > before + 128 * 1024 || resident - after < size - 128 * 1024) return 9;
    printf("CRUN_MEMORY_REAL_CHARGE_REFUND_OK before=%llu resident=%llu after=%llu\n",
        (unsigned long long)before, (unsigned long long)resident, (unsigned long long)after);
    return 0;
}
int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "pids")) return pids_probe();
    if (argc == 2 && !strcmp(argv[1], "memory")) return memory_probe(0);
    if (argc == 2 && !strcmp(argv[1], "oom")) return memory_probe(1);
    return 2;
}
