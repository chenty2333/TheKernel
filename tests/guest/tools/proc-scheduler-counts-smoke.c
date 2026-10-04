#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int snapshot(unsigned long long *switches, unsigned long long *tasks) {
    FILE *f = fopen("/proc/stat", "r");
    if (!f) return 1;
    char line[8192];
    unsigned int found = 0;
    unsigned long long n;
    while (fgets(line, sizeof(line), f)) {
        if (sscanf(line, "ctxt %llu", &n) == 1) { *switches = n; found |= 1; }
        if (sscanf(line, "processes %llu", &n) == 1) { *tasks = n; found |= 2; }
        if (sscanf(line, "procs_running %llu", &n) == 1) {
            if (!n) return 1;
            found |= 4;
        }
    }
    if (fclose(f) || found != 7) return 1;
    return 0;
}
int main(void) {
    unsigned long long switches_before = 0, tasks_before = 0, switches_after = 0, tasks_after = 0;
    int pipefd[2];
    if (pipe(pipefd) || snapshot(&switches_before, &tasks_before)) return 1;
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        close(pipefd[0]);
        usleep(20000);
        char value = 'x';
        if (write(pipefd[1], &value, 1) != 1) _exit(1);
        _exit(0);
    }
    close(pipefd[1]);
    char value;
    if (read(pipefd[0], &value, 1) != 1 || value != 'x' || close(pipefd[0])) return 1;
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 1;
    if (snapshot(&switches_after, &tasks_after) || switches_after <= switches_before || tasks_after <= tasks_before) return 1;
    printf("PROC_SCHEDULER_COUNTS_OK switches_delta=%llu publications_delta=%llu\n",
           switches_after-switches_before,tasks_after-tasks_before);
    return 0;
}
