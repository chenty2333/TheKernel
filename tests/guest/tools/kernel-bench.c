#define _GNU_SOURCE
/* Identical benchmark executable for TheKernel and Linux. Measurements report
 * observations only, not scheduler wins or substitutes for KVM trials.
 * The diagnostics command is an opt-in TheKernel-specific regression. */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/io_uring.h>
#include <linux/magic.h>
#include <linux/perf_event.h>
#include <signal.h>
#include <sched.h>
#include <stdint.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/ioctl.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define MAX_QD 32U
#define FILE_BYTES (16U * 1024U * 1024U)
#define WARMUP 128U

static void fail(const char *what)
{
    perror(what);
    exit(1);
}

/* Both guests must measure the rootdisk provider, never a tmpfs overlay. */
static void require_ext4(int fd)
{
    struct statfs fs;
    if (fstatfs(fd, &fs)) fail("benchmark fstatfs");
    if ((unsigned long)fs.f_type != EXT4_SUPER_MAGIC) {
        errno = EXDEV;
        fail("benchmark file must reside on ext4 rootdisk");
    }
}

static uint64_t now_ns(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) fail("clock_gettime");
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_nsec;
}

/* Task-local counters: neither inherited children nor background work is charged
 * to the foreground caller. maxRSS is an absolute process lifetime high-water. */
struct measurement {
    int fd;
    struct rusage before, after;
    uint64_t migrations;
};

static void measurement_open(struct measurement *m)
{
    struct perf_event_attr attr = {0};
    attr.size = sizeof(attr);
    attr.type = PERF_TYPE_SOFTWARE;
    attr.config = PERF_COUNT_SW_CPU_MIGRATIONS;
    attr.disabled = 1;
    m->fd = syscall(SYS_perf_event_open, &attr, 0, -1, -1, PERF_FLAG_FD_CLOEXEC);
    if (m->fd < 0) fail("perf CPU migrations (required, no fallback)");
}

static void measurement_start(struct measurement *m)
{
    if (getrusage(RUSAGE_SELF, &m->before)) fail("measurement getrusage");
    if (ioctl(m->fd, PERF_EVENT_IOC_RESET, 0) || ioctl(m->fd, PERF_EVENT_IOC_ENABLE, 0))
        fail("enable migration counter");
}

static void measurement_stop(struct measurement *m)
{
    if (ioctl(m->fd, PERF_EVENT_IOC_DISABLE, 0)) fail("disable migration counter");
    if (getrusage(RUSAGE_SELF, &m->after)) fail("measurement getrusage");
    if (read(m->fd, &m->migrations, sizeof(m->migrations)) != sizeof(m->migrations))
        fail("read migration counter");
    if (close(m->fd)) fail("close migration counter");
}

static int lifecycle_counter(int pid, int cpu)
{
    struct perf_event_attr attr = {0};
    attr.size = sizeof(attr);
    attr.type = PERF_TYPE_SOFTWARE;
    attr.config = PERF_COUNT_SW_CPU_MIGRATIONS;
    attr.disabled = 1;
    int fd = syscall(SYS_perf_event_open, &attr, pid, cpu, -1, PERF_FLAG_FD_CLOEXEC);
    if (fd < 0) fail("perf lifecycle open");
    if (ioctl(fd, PERF_EVENT_IOC_ENABLE, 0)) fail("perf lifecycle enable");
    return fd;
}

static void perf_lifecycle(void)
{
    int cpu = sched_getcpu();
    if (cpu < 0) fail("perf lifecycle CPU");
    for (unsigned systemwide = 0; systemwide < 2; systemwide++) {
        int pid = systemwide ? -1 : 0;
        int target_cpu = systemwide ? cpu : -1;
        int sentinel = lifecycle_counter(pid, target_cpu);
        /* Keep a live group while recycling four times the 64-group quota.
         * No yield, sleep, fork or diagnostic output can supply a cleanup
         * boundary between close and the next open. */
        for (unsigned i = 0; i < 256; i++) {
            int fd = lifecycle_counter(pid, target_cpu);
            if (ioctl(fd, PERF_EVENT_IOC_DISABLE, 0)) fail("perf lifecycle disable");
            if (close(fd)) fail("perf lifecycle close");
        }
        uint64_t value;
        if (read(sentinel, &value, sizeof(value)) != sizeof(value))
            fail("perf lifecycle surviving group read");
        if (ioctl(sentinel, PERF_EVENT_IOC_DISABLE, 0) || close(sentinel))
            fail("perf lifecycle surviving group close");
        printf("{\"suite\":\"perf-lifecycle\",\"scope\":\"%s\",\"iterations\":256,"
               "\"survivor_read\":true}\n", systemwide ? "cpu" : "task");
    }
}

static uint64_t timeval_ns(struct timeval t)
{
    return (uint64_t)t.tv_sec * 1000000000ULL + (uint64_t)t.tv_usec * 1000ULL;
}

static void measurement_print(const struct measurement *m)
{
    printf(",\"measurement\":{\"scope\":\"foreground_caller\","
           "\"cpu_user_ns\":%" PRIu64 ",\"cpu_system_ns\":%" PRIu64
           ",\"voluntary_switches\":%ld,\"involuntary_switches\":%ld,"
           "\"cpu_migrations\":%" PRIu64 ",\"maxrss_kib\":%ld}",
           timeval_ns(m->after.ru_utime) - timeval_ns(m->before.ru_utime),
           timeval_ns(m->after.ru_stime) - timeval_ns(m->before.ru_stime),
           m->after.ru_nvcsw - m->before.ru_nvcsw,
           m->after.ru_nivcsw - m->before.ru_nivcsw, m->migrations, m->after.ru_maxrss);
}

static int compare_u64(const void *a, const void *b);

#define TRACE_CPUS 64U
#define TRACE_PAGES 64U
struct trace_ring {
    int fd;
    struct perf_event_mmap_page *mapping;
    size_t bytes, offset, size;
    unsigned pid_offset, prev_pid_offset, prev_state_offset, kind;
    uint64_t records;
};
struct trace_edge { uint64_t time; unsigned kind; };
struct wake_trace {
    struct trace_ring rings[TRACE_CPUS * 2];
    unsigned ring_count;
    pid_t target;
    struct trace_edge *edges;
    size_t count, capacity;
    uint64_t start, end, latest_block, latest_runnable, quantiles[3];
    size_t samples;
};

static unsigned trace_field(const char *event, const char *field, unsigned expected_size)
{
    char path[160], format[8192];
    snprintf(path, sizeof(path), "/sys/kernel/tracing/events/sched/%s/format", event);
    FILE *f = fopen(path, "r");
    if (!f) fail("tracepoint format (required)");
    size_t n = fread(format, 1, sizeof(format) - 1, f);
    if (ferror(f) || !feof(f)) { errno = EOVERFLOW; fail("tracepoint format size"); }
    fclose(f);
    format[n] = 0;
    char *match = strstr(format, field), *offset = match ? strstr(match, "offset:") : NULL;
    char *size = offset ? strstr(offset, "size:") : NULL;
    unsigned o, z;
    if (!match || !offset || !size ||
        (strchr(match, '\n') && size > strchr(match, '\n')) ||
        sscanf(offset, "offset:%u;", &o) != 1 || sscanf(size, "size:%u;", &z) != 1 || z != expected_size || o > 128) {
        errno = EINVAL; fail("tracepoint field layout");
    }
    return o;
}

static void wake_trace_open(struct wake_trace *trace, pid_t child, unsigned iterations)
{
    memset(trace, 0, sizeof(*trace));
    trace->target = child;
    trace->capacity = (size_t)iterations * 8 + 1024;
    trace->edges = calloc(trace->capacity, sizeof(*trace->edges));
    if (!trace->edges) fail("trace edge buffer");
    const char *events[] = {"sched_wakeup", "sched_switch"};
    long cpus = sysconf(_SC_NPROCESSORS_ONLN), page = sysconf(_SC_PAGESIZE);
    if (cpus < 1 || cpus > TRACE_CPUS || page <= 0) { errno = EINVAL; fail("trace CPU topology"); }
    for (unsigned kind = 0; kind < 2; kind++) {
        char path[160];
        snprintf(path, sizeof(path), "/sys/kernel/tracing/events/sched/%s/id", events[kind]);
        FILE *f = fopen(path, "r");
        unsigned long long id;
        if (!f) fail("tracepoint ID (required)");
        if (fscanf(f, "%llu", &id) != 1) { errno = EINVAL; fail("tracepoint ID"); }
        fclose(f);
        unsigned offset = trace_field(events[kind], kind ? " next_pid;" : " pid;", 4);
        unsigned prev_pid = kind ? trace_field(events[kind], " prev_pid;", 4) : 0;
        unsigned prev_state = kind ? trace_field(events[kind], " prev_state;", 8) : 0;
        for (long cpu = 0; cpu < cpus; cpu++) {
            struct trace_ring *ring = &trace->rings[trace->ring_count++];
            struct perf_event_attr attr = {0};
            attr.size = sizeof(attr);
            attr.type = PERF_TYPE_TRACEPOINT;
            attr.config = id;
            attr.sample_period = 1;
            attr.read_format = PERF_FORMAT_LOST;
            attr.sample_type = PERF_SAMPLE_TIME | PERF_SAMPLE_RAW;
            attr.disabled = 1;
            attr.use_clockid = 1;
            attr.clockid = CLOCK_MONOTONIC;
            /* Poll/drain explicitly: no sampler task is introduced. */
            attr.watermark = 1;
            attr.wakeup_watermark = (unsigned)(page * TRACE_PAGES / 2);
            ring->fd = syscall(SYS_perf_event_open, &attr, -1, cpu, -1, PERF_FLAG_FD_CLOEXEC);
            if (ring->fd < 0) fail("scheduler tracepoint perf_event_open (required)");
            ring->bytes = (TRACE_PAGES + 1) * (size_t)page;
            ring->mapping = mmap(NULL, ring->bytes, PROT_READ | PROT_WRITE, MAP_SHARED, ring->fd, 0);
            if (ring->mapping == MAP_FAILED) fail("scheduler tracepoint mmap");
            ring->offset = ring->mapping->data_offset;
            ring->size = ring->mapping->data_size;
            ring->kind = kind;
            ring->pid_offset = offset;
            ring->prev_pid_offset = prev_pid;
            ring->prev_state_offset = prev_state;
            if (!ring->size || (ring->size & (ring->size - 1)) ||
                ring->offset > ring->bytes || ring->size > ring->bytes - ring->offset) {
                errno = EINVAL; fail("scheduler trace ring layout");
            }
        }
    }
}

static void trace_copy(struct trace_ring *ring, uint64_t tail, void *out, size_t size)
{
    size_t pos = tail & (ring->size - 1);
    size_t first = ring->size - pos < size ? ring->size - pos : size;
    char *data = (char *)ring->mapping + ring->offset;
    memcpy(out, data + pos, first);
    memcpy((char *)out + first, data, size - first);
}

static void wake_trace_drain(struct wake_trace *trace)
{
    for (unsigned i = 0; i < trace->ring_count; i++) {
        struct trace_ring *ring = &trace->rings[i];
        uint64_t head = __atomic_load_n(&ring->mapping->data_head, __ATOMIC_ACQUIRE);
        uint64_t tail = __atomic_load_n(&ring->mapping->data_tail, __ATOMIC_RELAXED);
        if (head - tail > ring->size) { errno = EOVERFLOW; fail("scheduler trace overrun"); }
        while (tail < head) {
            struct perf_event_header header;
            if (head - tail < sizeof(header)) { errno = EIO; fail("partial trace header"); }
            trace_copy(ring, tail, &header, sizeof(header));
            if (header.size < sizeof(header) || header.size > head - tail) {
                errno = EIO; fail("partial trace record");
            }
            if (header.type == PERF_RECORD_LOST || header.type == PERF_RECORD_LOST_SAMPLES) {
                errno = EOVERFLOW; fail("lost scheduler samples");
            }
            if (header.type == PERF_RECORD_SAMPLE) {
                ring->records++;
                unsigned char record[256];
                uint64_t timestamp;
                uint32_t raw_size, pid;
                if (header.size < 20 || header.size > sizeof(record)) { errno = EIO; fail("trace sample size"); }
                trace_copy(ring, tail, record, header.size);
                memcpy(&timestamp, record + 8, 8);
                memcpy(&raw_size, record + 16, 4);
                if (raw_size > header.size - 20U || raw_size < ring->pid_offset + 4) {
                    errno = EIO; fail("trace raw payload");
                }
                if (ring->kind) {
                    uint32_t previous_pid;
                    uint64_t previous_state;
                    if (raw_size < ring->prev_pid_offset + 4 || raw_size < ring->prev_state_offset + 8) {
                        errno = EIO; fail("trace switch-out payload");
                    }
                    memcpy(&previous_pid, record + 20 + ring->prev_pid_offset, 4);
                    memcpy(&previous_state, record + 20 + ring->prev_state_offset, 8);
                    if (previous_pid == (uint32_t)trace->target && previous_state == 1 &&
                        timestamp > trace->latest_block)
                        trace->latest_block = timestamp;
                }
                memcpy(&pid, record + 20 + ring->pid_offset, 4);
                if (pid == (uint32_t)trace->target && timestamp > trace->latest_runnable)
                    trace->latest_runnable = timestamp;
                if (pid == (uint32_t)trace->target && trace->start && timestamp >= trace->start) {
                    if (trace->count == trace->capacity) { errno = EOVERFLOW; fail("target trace capacity"); }
                    trace->edges[trace->count++] = (struct trace_edge){timestamp, ring->kind};
                }
            } else if (header.type != PERF_RECORD_THROTTLE && header.type != PERF_RECORD_UNTHROTTLE) {
                errno = EIO; fail("unexpected scheduler trace record");
            } else {
                errno = EOVERFLOW; fail("throttled scheduler trace");
            }
            tail += header.size;
        }
        __atomic_store_n(&ring->mapping->data_tail, tail, __ATOMIC_RELEASE);
    }
}

static void wake_trace_enable(struct wake_trace *trace)
{
    for (unsigned i = 0; i < trace->ring_count; i++)
        if (ioctl(trace->rings[i].fd, PERF_EVENT_IOC_ENABLE, 0)) fail("enable scheduler trace");
}

static void wake_trace_wait_block(struct wake_trace *trace, uint64_t last_sent)
{
    uint64_t deadline = now_ns() + 10000000000ULL;
    for (;;) {
        wake_trace_drain(trace);
        if (trace->latest_block > last_sent && trace->latest_block > trace->latest_runnable) return;
        if (now_ns() >= deadline) {
            fprintf(stderr, "handoff block gate: target=%ld last_sent=%" PRIu64
                    " latest_block=%" PRIu64 " latest_runnable=%" PRIu64 "\n",
                    (long)trace->target, last_sent, trace->latest_block, trace->latest_runnable);
            errno = ETIMEDOUT; fail("wait for handoff child blocking switch");
        }
        // Let a runnable child finish its previous reply and enter the empty
        // request read. A fresh blocking switch, never elapsed time, opens the gate.
        if (sched_yield()) fail("yield for handoff child block");
    }
}

static int compare_edges(const void *a, const void *b)
{
    const struct trace_edge *x = a, *y = b;
    if (x->time != y->time) return (x->time > y->time) - (x->time < y->time);
    return (x->kind > y->kind) - (x->kind < y->kind);
}

static void wake_trace_stop(struct wake_trace *trace)
{
    trace->end = now_ns();
    for (unsigned i = 0; i < trace->ring_count; i++)
        if (ioctl(trace->rings[i].fd, PERF_EVENT_IOC_DISABLE, 0)) fail("disable scheduler trace");
    wake_trace_drain(trace);
    for (unsigned i = 0; i < trace->ring_count; i++) {
        uint64_t counts[2];
        if (read(trace->rings[i].fd, counts, sizeof(counts)) != sizeof(counts))
            fail("scheduler trace final count");
        if (counts[1] || counts[0] != trace->rings[i].records) {
            errno = EOVERFLOW; fail("lost or unaccounted scheduler trace records");
        }
    }
    qsort(trace->edges, trace->count, sizeof(*trace->edges), compare_edges);
    uint64_t *latencies = calloc(trace->count ? trace->count : 1, sizeof(*latencies));
    if (!latencies) fail("wake latency samples");
    uint64_t wake = 0;
    size_t window_wakes = 0, window_switches = 0, outside_window = 0;
    for (size_t i = 0; i < trace->count; i++) {
        struct trace_edge edge = trace->edges[i];
        if (edge.time < trace->start || edge.time > trace->end) {
            outside_window++;
            continue;
        }
        if (!edge.kind) {
            window_wakes++;
            if (wake) { errno = EIO; fail("duplicate unmatched target wake"); }
            wake = edge.time;
        } else {
            window_switches++;
            if (wake) {
                latencies[trace->samples++] = edge.time - wake;
                wake = 0;
            }
        }
    }
    if (!trace->samples || wake) {
        fprintf(stderr, "wake trace incomplete: target=%ld edges=%zu pairs=%zu "
                "window_wakes=%zu window_switches=%zu outside=%zu pending=%" PRIu64
                " start=%" PRIu64 " end=%" PRIu64 "\n",
                (long)trace->target, trace->count, trace->samples,
                window_wakes, window_switches, outside_window, wake, trace->start, trace->end);
        for (unsigned i = 0; i < trace->ring_count; i++)
            fprintf(stderr, "wake trace ring=%u kind=%u records=%" PRIu64 "\n",
                    i, trace->rings[i].kind, trace->rings[i].records);
        for (size_t i = 0; i < trace->count; i++) {
            if (i >= 4 && trace->count - i > 4) continue;
            fprintf(stderr, "wake trace edge=%zu kind=%u time=%" PRIu64 "\n",
                    i, trace->edges[i].kind, trace->edges[i].time);
        }
        errno = EIO; fail("incomplete target wake trace");
    }
    qsort(latencies, trace->samples, sizeof(*latencies), compare_u64);
    const unsigned q[] = {50, 95, 99};
    for (unsigned i = 0; i < 3; i++) trace->quantiles[i] = latencies[(trace->samples * q[i] + 99) / 100 - 1];
    free(latencies);
    free(trace->edges);
    for (unsigned i = 0; i < trace->ring_count; i++) {
        if (munmap(trace->rings[i].mapping, trace->rings[i].bytes) || close(trace->rings[i].fd))
            fail("close scheduler trace");
    }
}

static void transfer(int fd, void *data, size_t size, int writing)
{
    size_t done = 0;
    while (done < size) {
        ssize_t n = writing ? write(fd, (char *)data + done, size - done)
                            : read(fd, (char *)data + done, size - done);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { if (!n) errno = EIO; fail("pipe transfer"); }
        done += (size_t)n;
    }
}

static int compare_u64(const void *a, const void *b)
{
    uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
    return (x > y) - (x < y);
}

#define MAX_PRESSURE_WORKERS 128U
/* Handler-visible state uses signal-safe scalar accesses. Publish each PID
 * before increasing the count so cleanup never observes an untracked slot. */
static volatile sig_atomic_t scheduler_children[MAX_PRESSURE_WORKERS + 1];
static volatile sig_atomic_t scheduler_child_count;

static void scheduler_cleanup(void)
{
    for (sig_atomic_t i = 0; i < scheduler_child_count; i++)
        if (scheduler_children[i] > 0) kill(scheduler_children[i], SIGKILL);
    for (sig_atomic_t i = 0; i < scheduler_child_count; i++)
        if (scheduler_children[i] > 0)
            while (waitpid(scheduler_children[i], NULL, 0) < 0 && errno == EINTR) {}
    scheduler_child_count = 0;
}

static void scheduler_timeout(int sig)
{
    for (sig_atomic_t i = 0; i < scheduler_child_count; i++)
        if (scheduler_children[i] > 0) kill(scheduler_children[i], SIGKILL);
    _exit(128 + sig);
}

struct pressure_state {
    unsigned phase, ready[MAX_PRESSURE_WORKERS];
    uint64_t start;
    uint64_t count[MAX_PRESSURE_WORKERS], max_gap[MAX_PRESSURE_WORKERS], last_progress[MAX_PRESSURE_WORKERS];
};

static void pressure_worker(struct pressure_state *state, unsigned id, int io, int file)
{
    /* A per-child deadline also covers termination of the parent by SIGKILL. */
    scheduler_child_count = 0;
    alarm(590);
    unsigned char block[4096], readback[4096];
    uint64_t generation = 0;
    volatile uint64_t computation = id + 1;
    for (;;) {
        unsigned phase = __atomic_load_n(&state->phase, __ATOMIC_SEQ_CST);
        if (phase == 2) _exit(0);
        if (io) {
            ++generation;
            for (unsigned j = 0; j < sizeof(block); j++)
                block[j] = (unsigned char)(j * 17U + id * 29U) ^
                           (unsigned char)(generation >> ((j % 8) * 8));
            off_t offset = (off_t)id * 4096;
            if (pwrite(file, block, sizeof(block), offset) != sizeof(block) ||
                fsync(file) || pread(file, readback, sizeof(readback), offset) != sizeof(readback) ||
                memcmp(block, readback, sizeof(block))) _exit(3);
        } else {
            for (unsigned j = 0; j < 65536; j++)
                computation = computation * 6364136223846793005ULL + 1;
        }
        __atomic_store_n(&state->ready[id], 1, __ATOMIC_RELEASE);
        uint64_t current = now_ns();
        if (phase == 1 && __atomic_load_n(&state->phase, __ATOMIC_SEQ_CST) == 1) {
            uint64_t last = state->last_progress[id] ? state->last_progress[id] : state->start;
            uint64_t gap = current - last;
            if (gap > state->max_gap[id]) state->max_gap[id] = gap;
            state->last_progress[id] = current;
            __atomic_fetch_add(&state->count[id], 1, __ATOMIC_RELAXED);
        }
    }
}

static void scheduler(unsigned iterations, const char *path)
{
    long detected = sysconf(_SC_NPROCESSORS_ONLN);
    unsigned workers = detected > 0 ? (detected > 64 ? 64 : (unsigned)detected) : 0;
    const char *configured = getenv("KERNEL_BENCH_WORKERS");
    if (configured) {
        char *end;
        errno = 0;
        unsigned long parsed = strtoul(configured, &end, 10);
        if (errno || !*configured || *end || parsed < 1 || parsed > 64) {
            errno = EINVAL; fail("KERNEL_BENCH_WORKERS must be in [1,64]");
        }
        workers = (unsigned)parsed;
    }
    if (!workers) { errno = EINVAL; fail("online CPU count"); }
    int file = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (file < 0) fail("create exclusive scheduler pressure file");
    require_ext4(file);
    if (unlink(path)) fail("unlink scheduler pressure file");
    if (ftruncate(file, MAX_PRESSURE_WORKERS * 4096)) fail("size pressure file");
    struct pressure_state *state = mmap(NULL, sizeof(*state), PROT_READ | PROT_WRITE,
                                       MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (state == MAP_FAILED) fail("pressure shared state");
    uint64_t *samples = calloc(iterations, sizeof(*samples));
    if (!samples) fail("calloc");
    if (atexit(scheduler_cleanup)) fail("atexit");
    signal(SIGALRM, scheduler_timeout);
    signal(SIGTERM, scheduler_timeout);
    signal(SIGINT, scheduler_timeout);
    signal(SIGPIPE, SIG_IGN);
    const char *pressures[] = {"none", "cpu", "io", "mixed"};
    for (unsigned mode = 0; mode < 4; mode++) {
        memset(state, 0, sizeof(*state));
        unsigned total = mode == 3 ? workers * 2 : (mode ? workers : 0);
        uint64_t warmup = now_ns();
        for (unsigned w = 0; w < total; w++) {
            pid_t pid = fork();
            if (pid < 0) fail("fork pressure worker");
            if (!pid) pressure_worker(state, w, mode == 2 || (mode == 3 && w >= workers), file);
            scheduler_children[scheduler_child_count] = pid;
            scheduler_child_count++;
        }
        for (;;) {
            unsigned ready = 0;
            for (unsigned w = 0; w < total; w++) {
                int status;
                if (waitpid(scheduler_children[w], &status, WNOHANG) != 0) {
                    scheduler_children[w] = 0;
                    errno = ECHILD; fail("pressure worker warmup");
                }
                ready += __atomic_load_n(&state->ready[w], __ATOMIC_ACQUIRE);
            }
            if (ready == total && now_ns() - warmup >= 100000000ULL) break;
            struct timespec pause = {0, 1000000}; nanosleep(&pause, NULL);
        }
        int request[2], reply[2], status;
        if (pipe(request) || pipe(reply)) fail("pipe");
        pid_t child = fork();
        if (child < 0) fail("fork handoff child");
        if (!child) {
            scheduler_child_count = 0;
            alarm(590);
            close(request[1]); close(reply[0]);
            for (unsigned i = 0; i < iterations + WARMUP; i++) {
                uint64_t sent;
                transfer(request[0], &sent, sizeof(sent), 0);
                uint64_t received = now_ns();
                if (received < sent) _exit(2);
                transfer(reply[1], &received, sizeof(received), 1);
            }
            _exit(0);
        }
        scheduler_children[scheduler_child_count] = child;
        scheduler_child_count++;
        close(request[0]); close(reply[1]);
        struct wake_trace trace;
        wake_trace_open(&trace, child, iterations);
        struct measurement measured;
        measurement_open(&measured);
        wake_trace_enable(&trace);
        uint64_t start = 0, last_sent = 0;
        for (unsigned i = 0; i < iterations + WARMUP; i++) {
            if (i == WARMUP) {
                trace.start = now_ns();
                measurement_start(&measured);
                start = now_ns();
                state->start = start;
                __atomic_store_n(&state->phase, 1, __ATOMIC_SEQ_CST);
            }
            // The first warmup send bootstraps a child which may have blocked
            // before tracing was enabled. Every later send requires a new
            // committed interruptible switch-out after the previous request.
            if (i) wake_trace_wait_block(&trace, last_sent);
            uint64_t sent = now_ns(), received;
            last_sent = sent;
            transfer(request[1], &sent, sizeof(sent), 1);
            transfer(reply[0], &received, sizeof(received), 0);
            if (received < sent) { errno = EIO; fail("nonmonotonic sample"); }
            if (i >= WARMUP) {
                samples[i - WARMUP] = received - sent;
                wake_trace_drain(&trace);
            }
        }
        uint64_t elapsed = now_ns() - start;
        measurement_stop(&measured);
        wake_trace_stop(&trace);
        close(request[1]); close(reply[0]);
        pid_t waited = waitpid(child, &status, 0);
        if (waited == child) scheduler_children[--scheduler_child_count] = 0;
        if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status)) {
            errno = ECHILD; fail("scheduler child");
        }
        while (now_ns() - start < 250000000ULL) {
            struct timespec pause = {0, 1000000}; nanosleep(&pause, NULL);
        }
        __atomic_store_n(&state->phase, 2, __ATOMIC_SEQ_CST);
        uint64_t end = now_ns();
        uint64_t pressure_elapsed = end - start;
        for (unsigned w = 0; w < total; w++) {
            pid_t pid = scheduler_children[w];
            waited = waitpid(pid, &status, 0);
            if (waited == pid) scheduler_children[w] = 0;
            if (waited != pid || !WIFEXITED(status) || WEXITSTATUS(status)) {
                errno = ECHILD; fail("pressure worker");
            }
        }
        scheduler_child_count = 0;
        unsigned zero = 0;
        for (unsigned w = 0; w < total; w++) {
            uint64_t count = state->count[w];
            zero += count == 0;
            uint64_t last = state->last_progress[w] ? state->last_progress[w] : start;
            uint64_t terminal_gap = end > last ? end - last : 0;
            if (terminal_gap > state->max_gap[w]) state->max_gap[w] = terminal_gap;
        }
        qsort(samples, iterations, sizeof(*samples), compare_u64);
        printf("{\"suite\":\"scheduler\",\"workload\":\"pipe_handoff\","
               "\"pressure\":\"%s\",\"workers_per_kind\":%u,\"background_cpu_workers\":%u,\"background_io_workers\":%u,"
               "\"iterations\":%u,\"elapsed_ns\":%" PRIu64 ",\"pressure_elapsed_ns\":%" PRIu64 ","
               "\"zero_progress_workers\":%u,"
               "\"handoff_p50_ns\":%" PRIu64 ",\"handoff_p95_ns\":%" PRIu64 ",\"handoff_p99_ns\":%" PRIu64 ",\"workers\":[",
               pressures[mode], workers, mode == 1 || mode == 3 ? workers : 0, mode == 2 || mode == 3 ? workers : 0, iterations, elapsed, pressure_elapsed,
               zero,
               samples[(iterations * 50 + 99) / 100 - 1], samples[(iterations * 95 + 99) / 100 - 1],
               samples[(iterations * 99 + 99) / 100 - 1]);
        for (unsigned w = 0; w < total; w++) {
            printf("%s{\"worker\":%u,\"kind\":\"%s\",\"units\":%" PRIu64
                   ",\"units_per_second\":%.3f,\"max_progress_gap_ns\":%" PRIu64 "}",
                   w ? "," : "", w,
                   mode == 2 || (mode == 3 && w >= workers) ? "write_fsync_read_4k" : "lcg_65536",
                   state->count[w], (double)state->count[w] * 1e9 / pressure_elapsed, state->max_gap[w]);
        }
        printf("]");
        measurement_print(&measured);
        printf(",\"wake_trace\":{\"scope\":\"handoff_child\",\"clock\":\"monotonic\","
               "\"samples\":%zu,\"wake_to_run_p50_ns\":%" PRIu64 ",\"wake_to_run_p95_ns\":%" PRIu64
               ",\"wake_to_run_p99_ns\":%" PRIu64 "}", trace.samples,
               trace.quantiles[0], trace.quantiles[1], trace.quantiles[2]);
        puts("}");
        fflush(stdout);
    }
    free(samples);
    munmap(state, sizeof(*state));
    close(file);
}

struct ring {
    int fd;
    void *sq, *cq;
    size_t sq_bytes, cq_bytes, sqe_bytes;
    struct io_uring_sqe *sqes;
    unsigned *sq_head, *sq_tail, *sq_mask, *array;
    unsigned *cq_head, *cq_tail, *cq_mask, *cq_overflow, *sq_dropped;
    struct io_uring_cqe *cqes;
};

/* Timeout diagnostics must not acquire stdio/allocator locks interrupted by
 * SIGALRM. x86_64 lock-free scalars also keep the measured loop free of calls. */
_Static_assert(ATOMIC_INT_LOCK_FREE == 2 && ATOMIC_POINTER_LOCK_FREE == 2,
               "I/O timeout snapshots require lock-free atomics");
enum io_phase { IO_SETUP, IO_WARMUP, IO_MEASURED, IO_READBACK, IO_CLEANUP };
enum io_step { IO_IDLE, IO_PREPARE, IO_SUBMIT, IO_WAIT, IO_VERIFY, IO_FSYNC };
static _Atomic unsigned io_phase, io_step, io_first, io_count, io_submitted, io_seen;
static _Atomic(unsigned *) io_counters[6];

/* The maximum line is below 1024 bytes: six 32-bit counters, six scalars and
 * at most MAX_QD two-digit user_data IDs. Append checks still bound every byte. */
static size_t timeout_text(char *line, size_t used, const char *text)
{
    while (*text && used < 1023) line[used++] = *text++;
    return used;
}

static size_t timeout_number(char *line, size_t used, unsigned value)
{
    char digits[10];
    unsigned count = 0;
    do { digits[count++] = (char)('0' + value % 10); value /= 10; } while (value);
    while (count && used < 1023) line[used++] = digits[--count];
    return used;
}

static void io_timeout(int sig)
{
    char line[1024];
    size_t used = timeout_text(line, 0, "THEKERNEL_IO_TIMEOUT phase=");
    unsigned phase = atomic_load_explicit(&io_phase, memory_order_relaxed);
    used = timeout_text(line, used, phase == IO_SETUP ? "setup" :
        phase == IO_WARMUP ? "warmup" : phase == IO_MEASURED ? "measured" :
        phase == IO_READBACK ? "readback" : "cleanup");
    unsigned step = atomic_load_explicit(&io_step, memory_order_relaxed);
    used = timeout_text(line, used, " step=");
    used = timeout_text(line, used, step == IO_PREPARE ? "prepare" :
        step == IO_SUBMIT ? "submit" : step == IO_WAIT ? "wait" :
        step == IO_VERIFY ? "verify" : step == IO_FSYNC ? "fsync" : "idle");
    unsigned seen = atomic_load_explicit(&io_seen, memory_order_relaxed);
    unsigned count = atomic_load_explicit(&io_count, memory_order_relaxed);
    unsigned completed = 0;
    for (unsigned bits = seen; bits; bits >>= 1) completed += bits & 1U;
#define NUMBER(label, value) do { \
    used = timeout_text(line, used, label); \
    used = timeout_number(line, used, (value)); \
} while (0)
    NUMBER(" batch_first=", atomic_load_explicit(&io_first, memory_order_relaxed));
    NUMBER(" count=", count);
    /* submitted is the count returned by enter, not a guess at an enter
     * currently interrupted in the kernel; SQ head/tail show that distinction. */
    NUMBER(" submitted=", atomic_load_explicit(&io_submitted, memory_order_relaxed));
    NUMBER(" completed=", completed);
    used = timeout_text(line, used, " missing_user_data=[");
    int comma = 0;
    for (unsigned i = 0; i < count && i < MAX_QD; i++) {
        if (seen & (1U << i)) continue;
        if (comma) used = timeout_text(line, used, ",");
        used = timeout_number(line, used, i);
        comma = 1;
    }
    used = timeout_text(line, used, "]");
    for (unsigned i = 0; i < 6; i++) {
        unsigned *counter = atomic_load_explicit(&io_counters[i], memory_order_relaxed);
        used = timeout_text(line, used, i == 0 ? " sq_head=" : i == 1 ? " sq_tail=" :
            i == 2 ? " cq_head=" : i == 3 ? " cq_tail=" :
            i == 4 ? " sq_dropped=" : " cq_overflow=");
        if (counter) used = timeout_number(line, used, __atomic_load_n(counter, __ATOMIC_ACQUIRE));
        else used = timeout_text(line, used, "unmapped");
    }
#undef NUMBER
    line[used++] = '\n';
    (void)write(STDERR_FILENO, line, used);
    _exit(128 + sig);
}

static void ring_open(struct ring *r, unsigned depth)
{
    struct io_uring_params p = {0};
    memset(r, 0, sizeof(*r));
    r->fd = (int)syscall(SYS_io_uring_setup, depth, &p);
    if (r->fd < 0) fail("io_uring_setup (required, no fallback)");
    r->sq_bytes = p.sq_off.array + p.sq_entries * sizeof(unsigned);
    r->cq_bytes = p.cq_off.cqes + p.cq_entries * sizeof(struct io_uring_cqe);
    if (p.features & IORING_FEAT_SINGLE_MMAP) {
        if (r->cq_bytes > r->sq_bytes) r->sq_bytes = r->cq_bytes;
        r->cq_bytes = r->sq_bytes;
    }
    r->sq = mmap(NULL, r->sq_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, r->fd, IORING_OFF_SQ_RING);
    if (r->sq == MAP_FAILED) fail("mmap SQ");
    r->cq = (p.features & IORING_FEAT_SINGLE_MMAP) ? r->sq :
        mmap(NULL, r->cq_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, r->fd, IORING_OFF_CQ_RING);
    if (r->cq == MAP_FAILED) fail("mmap CQ");
    r->sqe_bytes = p.sq_entries * sizeof(struct io_uring_sqe);
    r->sqes = mmap(NULL, r->sqe_bytes, PROT_READ | PROT_WRITE, MAP_SHARED, r->fd, IORING_OFF_SQES);
    if (r->sqes == MAP_FAILED) fail("mmap SQEs");
#define FIELD(base, off) ((unsigned *)((char *)(base) + (off)))
    r->sq_head = FIELD(r->sq, p.sq_off.head);
    r->sq_tail = FIELD(r->sq, p.sq_off.tail);
    r->sq_mask = FIELD(r->sq, p.sq_off.ring_mask);
    r->array = FIELD(r->sq, p.sq_off.array);
    r->sq_dropped = FIELD(r->sq, p.sq_off.dropped);
    r->cq_head = FIELD(r->cq, p.cq_off.head);
    r->cq_tail = FIELD(r->cq, p.cq_off.tail);
    r->cq_mask = FIELD(r->cq, p.cq_off.ring_mask);
    r->cq_overflow = FIELD(r->cq, p.cq_off.overflow);
    r->cqes = (struct io_uring_cqe *)((char *)r->cq + p.cq_off.cqes);
    unsigned *counters[] = {r->sq_head, r->sq_tail, r->cq_head, r->cq_tail,
                            r->sq_dropped, r->cq_overflow};
    for (unsigned i = 0; i < 6; i++)
        atomic_store_explicit(&io_counters[i], counters[i], memory_order_relaxed);
#undef FIELD
}

static void ring_close(struct ring *r)
{
    /* All CQEs were consumed before releasing buffers or the registered file. */
    if (close(r->fd)) fail("close ring");
    /* Clear every handler address before the first unmap. */
    for (unsigned i = 0; i < 6; i++)
        atomic_store_explicit(&io_counters[i], NULL, memory_order_relaxed);
    if (munmap(r->sqes, r->sqe_bytes)) fail("unmap SQEs");
    if (r->cq != r->sq && munmap(r->cq, r->cq_bytes)) fail("unmap CQ");
    if (munmap(r->sq, r->sq_bytes)) fail("unmap SQ");
}

/* Per-page generations make dropped writes observable even on a warm file. */
static uint32_t page_generations[FILE_BYTES / 4096U];
static uint32_t next_generation = 1;

static unsigned char data_byte(uint64_t offset)
{
    uint32_t generation = page_generations[offset / 4096U];
    return (unsigned char)(offset * 17U + (offset >> 12) * 29U + 3U) ^
           (unsigned char)(generation >> ((offset % 4U) * 8U));
}

static void io_batch(struct ring *r, int file, char **buffers, unsigned size,
                     unsigned count, unsigned first, int random, int fixed, int writing)
{
    atomic_store_explicit(&io_step, IO_PREPARE, memory_order_relaxed);
    atomic_store_explicit(&io_count, 0, memory_order_relaxed);
    atomic_store_explicit(&io_seen, 0, memory_order_relaxed);
    atomic_store_explicit(&io_submitted, 0, memory_order_relaxed);
    atomic_store_explicit(&io_first, first, memory_order_relaxed);
    atomic_store_explicit(&io_count, count, memory_order_relaxed);
    unsigned tail = __atomic_load_n(r->sq_tail, __ATOMIC_RELAXED);
    unsigned head = __atomic_load_n(r->sq_head, __ATOMIC_ACQUIRE);
    if (tail - head + count > *r->sq_mask + 1) { errno = EOVERFLOW; fail("SQ capacity"); }
    for (unsigned i = 0; i < count; i++) {
        unsigned index = (tail + i) & *r->sq_mask;
        struct io_uring_sqe *s = &r->sqes[index];
        memset(s, 0, sizeof(*s));
        s->opcode = fixed ? (writing ? IORING_OP_WRITE_FIXED : IORING_OP_READ_FIXED)
                          : (writing ? IORING_OP_WRITE : IORING_OP_READ);
        s->fd = fixed ? 0 : file;
        s->flags = fixed ? IOSQE_FIXED_FILE : 0;
        /* Odd multiplier permutes power-of-two block counts deterministically;
         * no overlapping blocks within a batch, identical across systems. */
        uint64_t block = (uint64_t)(first + i) * (random ? 2654435761U : 1U);
        s->off = (block % (FILE_BYTES / size)) * size;
        s->addr = (uintptr_t)buffers[i];
        s->len = size;
        s->buf_index = fixed ? (uint16_t)i : 0;
        s->user_data = i;
        r->array[index] = index;
        if (!writing) memset(buffers[i], 0, size);
        else {
            uint32_t generation = next_generation++;
            for (unsigned j = 0; j < size; j += 4096U)
                page_generations[(s->off + j) / 4096U] = generation;
            for (unsigned j = 0; j < size; j++) buffers[i][j] = (char)data_byte(s->off + j);
        }
    }
    __atomic_store_n(r->sq_tail, tail + count, __ATOMIC_RELEASE);
    unsigned submitted = 0;
    atomic_store_explicit(&io_step, IO_SUBMIT, memory_order_relaxed);
    while (submitted < count) {
        long n = syscall(SYS_io_uring_enter, r->fd, count - submitted, 0, 0, NULL, 0);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) { if (!n) errno = EIO; fail("io_uring_enter submit"); }
        if ((unsigned long)n > count - submitted) { errno = EIO; fail("excess I/O submissions"); }
        submitted += (unsigned)n;
        atomic_store_explicit(&io_submitted, submitted, memory_order_relaxed);
    }
    uint32_t seen = 0;
    for (unsigned done = 0; done < count;) {
        unsigned h = __atomic_load_n(r->cq_head, __ATOMIC_RELAXED);
        unsigned t = __atomic_load_n(r->cq_tail, __ATOMIC_ACQUIRE);
        if (h == t) {
            atomic_store_explicit(&io_step, IO_WAIT, memory_order_relaxed);
            if (syscall(SYS_io_uring_enter, r->fd, 0, 1, IORING_ENTER_GETEVENTS, NULL, 0) < 0 && errno != EINTR)
                fail("io_uring_enter wait");
            continue;
        }
        atomic_store_explicit(&io_step, IO_VERIFY, memory_order_relaxed);
        struct io_uring_cqe c = r->cqes[h & *r->cq_mask];
        __atomic_store_n(r->cq_head, h + 1, __ATOMIC_RELEASE);
        if (c.res != (int)size || c.user_data >= count || (seen & (1U << c.user_data))) {
            errno = c.res < 0 ? -c.res : EIO; fail("invalid I/O completion");
        }
        seen |= 1U << c.user_data;
        atomic_store_explicit(&io_seen, seen, memory_order_relaxed);
        if (!writing) {
            for (unsigned j = 0; j < size; j++)
                if ((unsigned char)buffers[c.user_data][j] !=
                    data_byte(r->sqes[(tail + (unsigned)c.user_data) & *r->sq_mask].off + j)) {
                    errno = EIO; fail("read integrity");
                }
        }
        done++;
    }
    if (__atomic_load_n(r->sq_dropped, __ATOMIC_ACQUIRE) ||
        __atomic_load_n(r->cq_overflow, __ATOMIC_ACQUIRE) ||
        __atomic_load_n(r->cq_head, __ATOMIC_ACQUIRE) !=
        __atomic_load_n(r->cq_tail, __ATOMIC_ACQUIRE)) {
        errno = EIO; fail("dropped submissions, CQ overflow or excess completions");
    }
    atomic_store_explicit(&io_step, IO_IDLE, memory_order_relaxed);
}

static void io_case(int file, unsigned iterations, unsigned size, unsigned depth,
                    int fixed, int direct, int mode)
{
    atomic_store_explicit(&io_phase, IO_SETUP, memory_order_relaxed);
    atomic_store_explicit(&io_count, 0, memory_order_relaxed);
    atomic_store_explicit(&io_seen, 0, memory_order_relaxed);
    atomic_store_explicit(&io_submitted, 0, memory_order_relaxed);
    atomic_store_explicit(&io_first, 0, memory_order_relaxed);
    printf("THEKERNEL_IO_BEGIN block_bytes=%u queue_depth=%u resources=%s cache=%s "
           "operation=%s durability=%s iterations=%u\n", size, depth,
           fixed ? "fixed" : "ordinary", direct ? "direct" : "buffered",
           mode ? "write" : "read", mode == 2 ? "fsync_per_batch" : "none", iterations);
    fflush(stdout);
    struct ring r;
    char *buffers[MAX_QD];
    struct iovec iov[MAX_QD];
    ring_open(&r, depth);
    for (unsigned i = 0; i < depth; i++) {
        int error = posix_memalign((void **)&buffers[i], 4096, size);
        if (error) { errno = error; fail("aligned buffer"); }
        for (unsigned j = 0; j < size; j++) buffers[i][j] = (char)(j * 17U + 3U);
        iov[i] = (struct iovec){buffers[i], size};
    }
    if (fixed) {
        if (syscall(SYS_io_uring_register, r.fd, IORING_REGISTER_FILES, &file, 1) < 0)
            fail("register files");
        if (syscall(SYS_io_uring_register, r.fd, IORING_REGISTER_BUFFERS, iov, depth) < 0)
            fail("register buffers");
    }
    struct measurement measured;
    measurement_open(&measured);
    uint64_t start = 0, elapsed = 0;
    for (unsigned phase = 0; phase < 2; phase++) {
        unsigned total = phase ? iterations : WARMUP;
        atomic_store_explicit(&io_phase, phase ? IO_MEASURED : IO_WARMUP, memory_order_relaxed);
        if (phase) { measurement_start(&measured); start = now_ns(); }
        for (unsigned n = 0; n < total;) {
            unsigned count = total - n < depth ? total - n : depth;
            io_batch(&r, file, buffers, size, count, n, size == 4096, fixed, mode != 0);
            /* Sync mode promises durability at each completed batch boundary,
             * not for every individual write within a queue-depth batch. */
            if (mode == 2) {
                atomic_store_explicit(&io_step, IO_FSYNC, memory_order_relaxed);
                if (fsync(file)) fail("fsync batch");
                atomic_store_explicit(&io_step, IO_IDLE, memory_order_relaxed);
            }
            n += count;
        }
        if (phase) { elapsed = now_ns() - start; measurement_stop(&measured); }
    }
    if (mode != 0) {
        /* Separate readback is outside write timing. */
        atomic_store_explicit(&io_phase, IO_READBACK, memory_order_relaxed);
        for (unsigned n = 0; n < iterations;) {
            unsigned count = iterations - n < depth ? iterations - n : depth;
            io_batch(&r, file, buffers, size, count, n, size == 4096, fixed, 0);
            n += count;
        }
    }
    atomic_store_explicit(&io_phase, IO_CLEANUP, memory_order_relaxed);
    printf("{\"suite\":\"io\",\"workload\":\"%s\",\"block_bytes\":%u,"
           "\"queue_depth\":%u,\"resources\":\"%s\",\"cache\":\"%s\","
           "\"operation\":\"%s\",\"durability\":\"%s\",\"iterations\":%u,"
           "\"includes_buffer_work\":true,\"elapsed_ns\":%" PRIu64 ",\"bytes\":%" PRIu64,
           size == 4096 ? "random" : "sequential", size, depth,
           fixed ? "fixed" : "ordinary", direct ? "direct" : "buffered",
           mode ? "write" : "read", mode == 2 ? "fsync_per_batch" : "none",
           iterations, elapsed, (uint64_t)iterations * size);
    measurement_print(&measured);
    puts("}");
    fflush(stdout);
    if (fixed) {
        if (syscall(SYS_io_uring_register, r.fd, IORING_UNREGISTER_BUFFERS, NULL, 0) < 0)
            fail("unregister buffers");
        if (syscall(SYS_io_uring_register, r.fd, IORING_UNREGISTER_FILES, NULL, 0) < 0)
            fail("unregister files");
    }
    ring_close(&r);
    for (unsigned i = 0; i < depth; i++) free(buffers[i]);
}

static void io_suite(unsigned iterations, const char *path)
{
    struct sigaction action = {0};
    action.sa_handler = io_timeout;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, NULL)) fail("I/O timeout handler");
    int file = open(path, O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (file < 0) fail("create exclusive benchmark file");
    require_ext4(file);
    int direct = open(path, O_RDWR | O_DIRECT | O_CLOEXEC);
    int saved = errno;
    if (unlink(path)) fail("unlink benchmark file");
    if (direct < 0) { errno = saved; fail("open O_DIRECT (required)"); }
    char *block;
    int error = posix_memalign((void **)&block, 4096, 128U * 1024U);
    if (error) { errno = error; fail("aligned setup buffer"); }
    for (unsigned off = 0; off < FILE_BYTES; off += 128U * 1024U) {
        for (unsigned j = 0; j < 128U * 1024U; j++) block[j] = (char)data_byte((uint64_t)off + j);
        ssize_t written = pwrite(file, block, 128U * 1024U, off);
        if (written != 128 * 1024) {
            if (written >= 0) errno = EIO;
            fail("initialize data");
        }
    }
    if (fsync(file)) fail("initialize fsync");
    free(block);
    const unsigned depths[] = {1, 8, 32}, sizes[] = {4096, 128U * 1024U};
    for (unsigned s = 0; s < 2; s++)
        for (unsigned q = 0; q < 3; q++)
            for (int fixed = 0; fixed < 2; fixed++)
                for (int d = 0; d < 2; d++)
                    for (int mode = 0; mode < 3; mode++) {
                        if (fsync(file)) fail("scenario boundary fsync");
                        io_case(d ? direct : file, iterations, sizes[s], depths[q], fixed, d, mode);
                    }
    close(direct); close(file);
}


/* Opt-in guest regression: real proc controls and syscall-generated records. */
#define DIAGNOSTICS_FILTER "/proc/sys/kernel/log_filter"
#define DIAGNOSTICS_STATS "/proc/sys/kernel/log_stats"
#define DIAGNOSTICS_BYTES (64U * 1024U)
static char diagnostics_saved_filter[2048];
static int diagnostics_restore_filter;
static int diagnostics_console_disabled;

static int diagnostics_write_filter(const char *text)
{
    int fd = open(DIAGNOSTICS_FILTER, O_WRONLY | O_CLOEXEC);
    if (fd < 0) return -1;
    size_t len = strlen(text);
    ssize_t n = write(fd, text, len);
    int saved = n < 0 ? errno : EIO;
    int closed = close(fd);
    if (n != (ssize_t)len) { errno = saved; return -1; }
    return closed;
}

static void diagnostics_cleanup(void)
{
    if (diagnostics_restore_filter &&
        diagnostics_write_filter(diagnostics_saved_filter))
        perror("diagnostics restore log_filter");
    /* The interface has no console-state query; re-enable it best effort. */
    if (diagnostics_console_disabled && syscall(SYS_syslog, 7, NULL, 0) < 0)
        perror("diagnostics restore console");
}

static void diagnostics_read_file(const char *path, char *text, size_t capacity)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) fail(path);
    size_t used = 0;
    for (;;) {
        ssize_t n = read(fd, text + used, capacity - 1 - used);
        if (n < 0) fail("diagnostics read control");
        if (!n) break;
        used += (size_t)n;
        if (used == capacity - 1) { errno = EOVERFLOW; fail(path); }
    }
    text[used] = '\0';
    if (close(fd)) fail("diagnostics close control");
}

static void diagnostics_require(int condition, const char *what)
{
    if (!condition) { errno = EPROTO; fail(what); }
}

static void diagnostics_expect_filter(const char *expected)
{
    char actual[2048];
    diagnostics_read_file(DIAGNOSTICS_FILTER, actual, sizeof(actual));
    diagnostics_require(!strcmp(actual, expected), "diagnostics filter readback");
}

static void diagnostics(void)
{
    static char retained[DIAGNOSTICS_BYTES + 1];
    const char *narrow = "off,thekernel_kernel::syscall=debug";
    char stats[2048];
    diagnostics_read_file(DIAGNOSTICS_FILTER, diagnostics_saved_filter,
                          sizeof(diagnostics_saved_filter));
    if (atexit(diagnostics_cleanup)) fail("diagnostics register cleanup");
    diagnostics_restore_filter = 1;
    if (diagnostics_write_filter(narrow)) fail("diagnostics set narrow filter");
    diagnostics_expect_filter("off,thekernel_kernel::syscall=debug\n");
    const char *invalid[] = {
        "trace,thekernel_kernel::syscall=not_a_level",
        "trace,thekernel_kernel::syscall=debug,thekernel_kernel::syscall=off",
        "trace,bad prefix=debug", "trace,"
    };
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        errno = 0;
        int result = diagnostics_write_filter(invalid[i]);
        diagnostics_require(result == -1 && errno == EINVAL,
                            "diagnostics invalid filter rejected");
        diagnostics_expect_filter("off,thekernel_kernel::syscall=debug\n");
    }
    /* Replacement must remove the old module override, not merge with it. */
    if (diagnostics_write_filter("off")) fail("diagnostics replace filter");
    diagnostics_expect_filter("off\n");
    int inherited = open(DIAGNOSTICS_FILTER, O_WRONLY | O_CLOEXEC);
    if (inherited < 0) fail("diagnostics privileged control open");
    pid_t child = fork();
    if (child < 0) fail("diagnostics fork");
    if (!child) {
        if (setuid(65534) || geteuid() != 65534) _exit(10);
        errno = 0;
        if (write(inherited, "trace", 5) != -1 ||
            (errno != EPERM && errno != EACCES)) _exit(11);
        errno = 0;
        int fd = open(DIAGNOSTICS_FILTER, O_WRONLY | O_CLOEXEC);
        if (fd >= 0) {
            if (write(fd, "trace", 5) != -1 ||
                (errno != EPERM && errno != EACCES)) _exit(12);
            close(fd);
        } else if (errno != EPERM && errno != EACCES) _exit(13);
        errno = 0;
        /* READ_ALL is public under the existing syslog policy; CLEAR always
         * requires privilege and cannot block waiting for a record. */
        if (syscall(SYS_syslog, 5, NULL, 0) != -1 || errno != EPERM) _exit(14);
        _exit(0);
    }
    if (close(inherited)) fail("diagnostics close inherited control");
    int status;
    if (waitpid(child, &status, 0) != child) fail("diagnostics wait child");
    if (!WIFEXITED(status) || WEXITSTATUS(status)) {
        fprintf(stderr, "diagnostics unprivileged child status=%d\n", status);
        errno = EPROTO;
        fail("diagnostics CAP_SYSLOG enforcement");
    }
    diagnostics_expect_filter("off\n");
    diagnostics_read_file(DIAGNOSTICS_STATS, stats, sizeof(stats));
    const char *fields[] = {
        "records_dropped", "diagnostic_records_dropped", "records_truncated",
        "retention_bytes_overwritten", "diagnostic_supported", "diagnostic_retired"
    };
    for (size_t i = 0; i < sizeof(fields) / sizeof(fields[0]); ++i) {
        char key[64];
        snprintf(key, sizeof(key), "%s ", fields[i]);
        const char *field = strstr(stats, key);
        diagnostics_require(field && (field == stats || field[-1] == '\n'),
                            "diagnostics stats field");
        field += strlen(key);
        char *end;
        errno = 0;
        unsigned long long value = strtoull(field, &end, 10);
        diagnostics_require(*field >= '0' && *field <= '9' && !errno &&
                            end != field && (*end == '\n' || !*end),
                            "diagnostics numeric stats");
        if (i >= 4) diagnostics_require(value <= 1, "diagnostics boolean stats");
    }
    long capacity = syscall(SYS_syslog, 10, NULL, 0);
    diagnostics_require(capacity > 0 && capacity <= DIAGNOSTICS_BYTES,
                        "diagnostics bounded syslog capacity");
    if (syscall(SYS_syslog, 6, NULL, 0) < 0) fail("diagnostics console off");
    diagnostics_console_disabled = 1;
    if (diagnostics_write_filter(narrow)) fail("diagnostics enable syscall log");
    long pid = syscall(SYS_getpid);
    if (pid <= 0) fail("diagnostics getpid");
    /* Stop production before taking the snapshot: the record must survive
     * filtering being disabled and diagnostic console suppression. */
    if (diagnostics_write_filter("off")) fail("diagnostics stop syscall log");
    long n = syscall(SYS_syslog, 3, retained, DIAGNOSTICS_BYTES);
    if (n < 0) fail("diagnostics read retained log");
    diagnostics_require(n <= DIAGNOSTICS_BYTES, "diagnostics syslog bound");
    retained[n] = '\0';
    char expected[256];
    snprintf(expected, sizeof(expected),
             " DEBUG target=thekernel_kernel::syscall module=thekernel_kernel::syscall] "
             "Syscall getpid return Ok(%ld)", pid);
    char *record = strstr(retained, expected);
    diagnostics_require(record != NULL, "diagnostics retained structured syscall record");
    char *start = record;
    while (start > retained && start[-1] != '\n') --start;
    diagnostics_require(!strncmp(start, "<7>[", 4), "diagnostics syslog debug priority");
    char *cpu = strstr(start, " cpu=");
    char *tid = strstr(start, " tid=");
    diagnostics_require(cpu && cpu < record && tid && tid < record,
                        "diagnostics retained execution metadata");
    if (diagnostics_write_filter(diagnostics_saved_filter))
        fail("diagnostics restore filter");
    diagnostics_restore_filter = 0;
    if (syscall(SYS_syslog, 7, NULL, 0) < 0) fail("diagnostics console on");
    diagnostics_console_disabled = 0;
    puts("THEKERNEL_LOG_DIAGNOSTICS_OK");
}

int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "diagnostics")) {
        diagnostics();
        return 0;
    }
    if (argc == 2 && !strcmp(argv[1], "perf-lifecycle")) {
        alarm(60);
        perf_lifecycle();
        return 0;
    }
    if (argc != 4) {
        fprintf(stderr, "usage: %s scheduler|io|all ITERATIONS EXCLUSIVE_DATA_FILE\n", argv[0]);
        fprintf(stderr, "       %s perf-lifecycle|diagnostics\n", argv[0]);
        return 2;
    }
    char *end;
    errno = 0;
    unsigned long count = strtoul(argv[2], &end, 10);
    if (errno || !*argv[2] || *end || count < MAX_QD || count > 1000000UL) {
        fprintf(stderr, "iterations must be an integer in [32, 1000000]\n"); return 2;
    }
    int sched = !strcmp(argv[1], "scheduler"), io = !strcmp(argv[1], "io");
    int all = !strcmp(argv[1], "all");
    if (!sched && !io && !all) {
        fprintf(stderr, "suite must be scheduler, io, or all\n"); return 2;
    }
    /* Bound a missing CQE or pipe handoff on both oracle and guest. */
    alarm(600);
    if (sched || all) scheduler((unsigned)count, argv[3]);
    if (io || all) io_suite((unsigned)count, argv[3]);
    return 0;
}
