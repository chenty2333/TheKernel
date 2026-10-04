#define _GNU_SOURCE
#include <ctype.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static int inspect(const char *path, unsigned int *cpus_out, unsigned long long *timer) {
    FILE *f = fopen(path, "r");
    if (!f) { perror(path); return 1; }
    char line[8192];
    if (!fgets(line, sizeof(line), f)) return 1;
    unsigned int cpus = 0;
    char *token = strtok(line, " \t\n");
    while (token) {
        unsigned int cpu; char suffix;
        if (sscanf(token, "CPU%u%c", &cpu, &suffix) != 1 || cpu != cpus++) return 1;
        token = strtok(NULL, " \t\n");
    }
    if (!cpus || cpus > 256) return 1;
    unsigned int rows = 0, local = 0, res = 0, tlb = 0, cal = 0;
    while (fgets(line, sizeof(line), f)) {
        char *colon = strchr(line, ':');
        if (!colon) return 1;
        *colon = 0;
        char *label = line;
        while (isspace((unsigned char)*label)) label++;
        char *at = colon + 1;
        unsigned long long sum = 0;
        unsigned int fields = !strcmp(label, "ERR") ? 1 : cpus;
        for (unsigned int i = 0; i < fields; i++) {
            while (isspace((unsigned char)*at)) at++;
            if (!isdigit((unsigned char)*at)) return 1;
            char *end;
            unsigned long long n = strtoull(at, &end, 10);
            if (end == at) return 1;
            sum += n; at = end;
        }
        if (!strcmp(label, "LOC")) { *timer = sum; local++; }
        res += !strcmp(label, "RES"); tlb += !strcmp(label, "TLB"); cal += !strcmp(label, "CAL");
        rows++;
    }
    int error = ferror(f);
    if (fclose(f) || error) return 1;
    if (strstr(path, "softirqs")) {
        if (rows != 10) return 1;
    } else if (local != 1 || res != 1 || tlb != 1 || cal != 1) return 1;
    *cpus_out = cpus;
    return 0;
}

int main(void) {
    unsigned int cpus, soft_cpus;
    unsigned long long before = 0, after = 0, unused = 0;
    if (inspect("/proc/interrupts", &cpus, &before) ||
        inspect("/proc/softirqs", &soft_cpus, &unused) || cpus != soft_cpus) return 1;
    struct timespec delay = { .tv_nsec = 50000000 };
    if (nanosleep(&delay, NULL) || inspect("/proc/interrupts", &cpus, &after)) return 1;
    if (after <= before) { fprintf(stderr, "local timer did not advance\n"); return 1; }
    printf("PROC_INTERRUPTS_OK cpus=%u timer_delta=%llu\n", cpus, after-before);
    return 0;
}
