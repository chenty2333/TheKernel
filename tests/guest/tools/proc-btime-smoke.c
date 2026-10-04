#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <time.h>

int main(void) {
    struct timespec before, after;
    if (clock_gettime(CLOCK_REALTIME, &before)) return 1;
    FILE *f = fopen("/proc/stat", "r");
    if (!f) return 1;
    char row[8192];
    unsigned int found = 0;
    unsigned long long boot = 0;
    while (fgets(row, sizeof(row), f)) {
        if (!strncmp(row, "btime ", 6)) {
            char extra;
            if (sscanf(row, "btime %llu %c", &boot, &extra) != 1) return 1;
            found++;
        }
    }
    int error = ferror(f);
    if (fclose(f) || error || found != 1 || !boot) return 1;
    f = fopen("/proc/uptime", "r");
    if (!f) return 1;
    double uptime;
    if (fscanf(f, "%lf", &uptime) != 1 || fclose(f)) return 1;
    if (clock_gettime(CLOCK_REALTIME, &after)) return 1;
    double sum = (double)boot+uptime;
    double lo = (double)before.tv_sec+(double)before.tv_nsec/1e9;
    double hi = (double)after.tv_sec+(double)after.tv_nsec/1e9;
    if (sum < lo-1.1 || sum > hi+1.1) {
        fprintf(stderr, "btime/uptime do not reconstruct wall time\n"); return 1;
    }
    puts("PROC_BTIME_OK");
    return 0;
}
