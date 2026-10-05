#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdatomic.h>
static atomic_int ready;
int release_gate;
__attribute__((noinline)) int worker_inner(int marker) {
    atomic_fetch_add(&ready, 1);
    while (!__atomic_load_n(&release_gate, __ATOMIC_ACQUIRE)) sched_yield();
    return marker + 1;
}
__attribute__((noinline)) void *worker_outer(void *argument) {
    return (void *)(uintptr_t)worker_inner((int)(uintptr_t)argument);
}
__attribute__((noinline)) int snapshot_ready(int marker) { return marker + 1; }
int main(void) {
    pthread_t first, second; void *a, *b;
    if (pthread_create(&first, NULL, worker_outer, (void *)(uintptr_t)11) ||
        pthread_create(&second, NULL, worker_outer, (void *)(uintptr_t)22)) return 2;
    while (atomic_load(&ready) != 2) sched_yield();
    int main_marker = snapshot_ready(33);
    if (pthread_join(first, &a) || pthread_join(second, &b)) return 3;
    printf("THREADS_RESULT=%lu,%lu,%d\n", (unsigned long)(uintptr_t)a,
           (unsigned long)(uintptr_t)b, main_marker);
    return (uintptr_t)a == 12 && (uintptr_t)b == 23 && main_marker == 34 ? 0 : 4;
}
