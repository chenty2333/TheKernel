#define _GNU_SOURCE

/*
 * Small, portable seccomp dispatch benchmark.
 *
 * The benchmark is deliberately a child-per-cell workload.  Installing a
 * seccomp filter and setting no_new_privs are irreversible for a thread, so a
 * failed or completed cell must never contaminate the next one.  Correctness
 * is checked and reported before any timing loop; timing samples are kept in
 * memory and emitted as one aggregate record after the loop.
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <stdbool.h>

#if !defined(__x86_64__)
#error "seccomp performance helper requires the x86_64 Linux ABI"
#endif

#ifndef SYS_seccomp
#define SYS_seccomp 317
#endif
#ifndef SECCOMP_SET_MODE_FILTER
#define SECCOMP_SET_MODE_FILTER 1U
#endif
#ifndef SECCOMP_MODE_FILTER
#define SECCOMP_MODE_FILTER 2U
#endif
#ifndef SECCOMP_RET_KILL_PROCESS
#define SECCOMP_RET_KILL_PROCESS 0x80000000U
#endif
#ifndef SECCOMP_RET_ALLOW
#define SECCOMP_RET_ALLOW 0x7fff0000U
#endif

#define PERF_SCHEMA "thekernel-perf-v1"
#define CASE_COUNT 4U
#define WARMUP_SAMPLES 32U
#define LATENCY_SAMPLES 128U
#define BRANCH_COUNT 256U
#define STACKED_COUNT 8U
#define UNKNOWN_SYSCALL_BASE 0x70000000U
#define BPF_CONTROL_PATH "/proc/bpf_executor_control"
#define BPF_STATS_PATH "/proc/bpf_stats"
#define BPF_STATS_BUFFER 4096U

enum executor_mode {
    EXECUTOR_AUTO,
    EXECUTOR_INTERPRETER,
    EXECUTOR_JIT,
};

struct bpf_counters {
    uint64_t published;
    uint64_t native_executed;
    uint64_t interpreter_executed;
    uint64_t fallback_policy_interpreter;
    uint64_t fallback_translation;
    uint64_t fallback_publication;
    uint64_t fallback_owner;
    uint64_t fallback_unavailable;
    uint64_t jit_rejected;
};

struct bpf_delta {
    struct bpf_counters values;
    bool available;
    bool valid;
};

struct executor_config {
    enum executor_mode mode;
    int control_state;
    int stats_state;
};

struct cell_proof {
    struct bpf_delta delta;
    const char *kind;
    const char *reason;
    bool accepted;
};

struct cell_result {
    uint64_t checksum;
    bool correctness_ok;
};

enum filter_case {
    CASE_NO_FILTER,
    CASE_SHORT,
    CASE_BRANCH_256,
    CASE_STACKED_8,
};

struct sample_set {
    uint64_t wall_ns[LATENCY_SAMPLES];
    uint64_t cpu_ns[LATENCY_SAMPLES];
};

static const char *case_name(enum filter_case value)
{
    switch (value) {
    case CASE_NO_FILTER:
        return "no-filter";
    case CASE_SHORT:
        return "short";
    case CASE_BRANCH_256:
        return "256branch";
    case CASE_STACKED_8:
        return "8-stacked";
    }
    return "unknown";
}

static void line_flush(void)
{
    fflush(stdout);
}

static void error_message(const char *stage)
{
    int saved_errno = errno;
    fprintf(stderr, "TKPERF_ERROR schema=%s workload=seccomp stage=%s errno=%d (%s)\n",
            PERF_SCHEMA, stage, saved_errno, strerror(saved_errno));
    errno = saved_errno;
}

static const char *executor_name(enum executor_mode mode)
{
    switch (mode) {
    case EXECUTOR_AUTO:
        return "auto";
    case EXECUTOR_INTERPRETER:
        return "interpreter";
    case EXECUTOR_JIT:
        return "jit";
    }
    return "unknown";
}

static int parse_executor(const char *text, enum executor_mode *mode)
{
    if (strcmp(text, "auto") == 0) {
        *mode = EXECUTOR_AUTO;
        return 0;
    }
    if (strcmp(text, "interpreter") == 0) {
        *mode = EXECUTOR_INTERPRETER;
        return 0;
    }
    if (strcmp(text, "jit") == 0) {
        *mode = EXECUTOR_JIT;
        return 0;
    }
    errno = EINVAL;
    return -1;
}

static int read_proc_text(const char *path, char *buffer, size_t capacity)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        if (errno == ENOENT || errno == ENOTDIR) {
            return 0;
        }
        return -1;
    }
    size_t length = 0;
    for (;;) {
        if (length + 1U >= capacity) {
            errno = EOVERFLOW;
            close(fd);
            return -1;
        }
        ssize_t count = read(fd, buffer + length, capacity - length - 1U);
        if (count == 0) {
            break;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            int saved_errno = errno;
            close(fd);
            errno = saved_errno;
            return -1;
        }
        length += (size_t)count;
    }
    int saved_errno = errno;
    if (close(fd) != 0) {
        return -1;
    }
    errno = saved_errno;
    buffer[length] = '\0';
    return 1;
}

static int write_all(int fd, const char *data, size_t length)
{
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = write(fd, data + offset, length - offset);
        if (count < 0 && errno == EINTR) {
            continue;
        }
        if (count <= 0) {
            return -1;
        }
        offset += (size_t)count;
    }
    return 0;
}

static int control_readback(const char *domain, enum executor_mode mode)
{
    char buffer[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_CONTROL_PATH, buffer, sizeof(buffer));
    if (result != 1) {
        return result;
    }
    char *save = NULL;
    bool found = false;
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        char key[32];
        char value[32];
        char extra[2];
        if (sscanf(line, "%31[^=]=%31s %1s", key, value, extra) != 2) {
            continue;
        }
        if (strcmp(key, domain) == 0) {
            found = strcmp(value, executor_name(mode)) == 0;
        }
    }
    return found ? 1 : -1;
}

static int set_executor_control(const char *domain, enum executor_mode mode)
{
    int fd = open(BPF_CONTROL_PATH, O_WRONLY);
    if (fd < 0) {
        if (errno == ENOENT || errno == ENOTDIR) {
            return 0;
        }
        return -1;
    }
    char request[64];
    int length = snprintf(request, sizeof(request), "%s=%s\n", domain,
                          executor_name(mode));
    if (length <= 0 || (size_t)length >= sizeof(request) ||
        write_all(fd, request, (size_t)length) != 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    if (close(fd) != 0) {
        return -1;
    }
    return control_readback(domain, mode);
}

static int parse_counter(const char *text, uint64_t *value)
{
    char *end = NULL;
    errno = 0;
    unsigned long long parsed = strtoull(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0') {
        return -1;
    }
    *value = (uint64_t)parsed;
    return 0;
}

static int read_bpf_stats(const char *domain, struct bpf_counters *counters)
{
    char buffer[BPF_STATS_BUFFER];
    int result = read_proc_text(BPF_STATS_PATH, buffer, sizeof(buffer));
    if (result != 1) {
        return result;
    }
    memset(counters, 0, sizeof(*counters));
    bool seen[9] = {false};
    char *save = NULL;
    size_t domain_length = strlen(domain);
    static const char stats_header[] = "BPF_STATS ";
    for (char *line = strtok_r(buffer, "\n", &save); line != NULL;
         line = strtok_r(NULL, "\n", &save)) {
        if (strncmp(line, stats_header, sizeof(stats_header) - 1U) == 0) {
            continue;
        }
        char key[64];
        char value[64];
        char extra[2];
        if (sscanf(line, "%63s %63s %1s", key, value, extra) != 2) {
            errno = EPROTO;
            return -1;
        }
        if (strncmp(key, domain, domain_length) != 0 ||
            key[domain_length] != '.') {
            continue;
        }
        const char *field = key + domain_length + 1U;
        uint64_t *destination = NULL;
        unsigned int index = 0;
        if (strcmp(field, "published") == 0) {
            destination = &counters->published;
            index = 0;
        } else if (strcmp(field, "native_executed") == 0) {
            destination = &counters->native_executed;
            index = 1;
        } else if (strcmp(field, "interpreter_executed") == 0) {
            destination = &counters->interpreter_executed;
            index = 2;
        } else if (strcmp(field, "fallback.policy_interpreter") == 0) {
            destination = &counters->fallback_policy_interpreter;
            index = 3;
        } else if (strcmp(field, "fallback.translation") == 0) {
            destination = &counters->fallback_translation;
            index = 4;
        } else if (strcmp(field, "fallback.publication") == 0) {
            destination = &counters->fallback_publication;
            index = 5;
        } else if (strcmp(field, "fallback.owner") == 0) {
            destination = &counters->fallback_owner;
            index = 6;
        } else if (strcmp(field, "fallback.unavailable") == 0) {
            destination = &counters->fallback_unavailable;
            index = 7;
        } else if (strcmp(field, "jit_rejected") == 0) {
            destination = &counters->jit_rejected;
            index = 8;
        }
        if (destination == NULL || seen[index] || parse_counter(value, destination) != 0) {
            errno = EPROTO;
            return -1;
        }
        seen[index] = true;
    }
    for (unsigned int index = 0; index < 9U; ++index) {
        if (!seen[index]) {
            errno = EPROTO;
            return -1;
        }
    }
    return 1;
}

static int prepare_executor(const char *domain, enum executor_mode mode,
                            struct executor_config *config)
{
    config->mode = mode;
    config->control_state = set_executor_control(domain, mode);
    config->stats_state = read_bpf_stats(domain, &(struct bpf_counters){0});
    if (config->control_state < 0 || config->stats_state < 0) {
        return -1;
    }
    if (mode != EXECUTOR_AUTO &&
        (config->control_state != 1 || config->stats_state != 1)) {
        errno = ENOTSUP;
        return 0;
    }
    return 1;
}

static bool subtract_counter(uint64_t after, uint64_t before, uint64_t *delta)
{
    if (after < before) {
        return false;
    }
    *delta = after - before;
    return true;
}

static bool make_delta(const struct bpf_counters *before,
                       const struct bpf_counters *after,
                       struct bpf_delta *delta)
{
    memset(delta, 0, sizeof(*delta));
    delta->available = true;
    delta->valid =
        subtract_counter(after->published, before->published,
                         &delta->values.published) &&
        subtract_counter(after->native_executed, before->native_executed,
                         &delta->values.native_executed) &&
        subtract_counter(after->interpreter_executed, before->interpreter_executed,
                         &delta->values.interpreter_executed) &&
        subtract_counter(after->fallback_policy_interpreter,
                         before->fallback_policy_interpreter,
                         &delta->values.fallback_policy_interpreter) &&
        subtract_counter(after->fallback_translation, before->fallback_translation,
                         &delta->values.fallback_translation) &&
        subtract_counter(after->fallback_publication, before->fallback_publication,
                         &delta->values.fallback_publication) &&
        subtract_counter(after->fallback_owner, before->fallback_owner,
                         &delta->values.fallback_owner) &&
        subtract_counter(after->fallback_unavailable, before->fallback_unavailable,
                         &delta->values.fallback_unavailable) &&
        subtract_counter(after->jit_rejected, before->jit_rejected,
                         &delta->values.jit_rejected);
    return delta->valid;
}

static uint64_t fallback_total(const struct bpf_counters *values)
{
    return values->fallback_policy_interpreter + values->fallback_translation +
           values->fallback_publication + values->fallback_owner +
           values->fallback_unavailable;
}

static struct cell_proof evaluate_proof(enum filter_case value,
                                        const struct executor_config *config,
                                        const struct bpf_delta *delta,
                                        bool correctness_ok)
{
    struct cell_proof proof = {.delta = *delta, .kind = "unsupported-ablation",
                               .reason = "bpf-stats-unavailable", .accepted = false};
    if (!correctness_ok && config->mode == EXECUTOR_JIT && delta->available &&
        delta->values.jit_rejected > 0) {
        proof.kind = "jit-rejected";
        proof.reason = "jit-rejected";
        return proof;
    }
    if (!correctness_ok) {
        proof.kind = "correctness-fail";
        proof.reason = "correctness-fail";
        return proof;
    }
    if (value == CASE_NO_FILTER) {
        proof.kind = "no-filter";
        proof.reason = "none";
        proof.accepted = true;
        return proof;
    }
    if (!delta->available) {
        if (config->mode == EXECUTOR_AUTO && config->stats_state == 0) {
            proof.kind = "linux-active/unsupported-ablation";
            proof.reason = "bpf-stats-unavailable";
            proof.accepted = true;
        }
        return proof;
    }
    if (!delta->valid) {
        proof.kind = "invalid-delta";
        proof.reason = "counter-regression";
        return proof;
    }
    const struct bpf_counters *d = &delta->values;
    uint64_t fallbacks = fallback_total(d);
    if (config->mode == EXECUTOR_INTERPRETER) {
        proof.kind = "verified";
        proof.reason = "none";
        proof.accepted = d->published > 0 && d->native_executed == 0 &&
                         d->interpreter_executed > 0 &&
                         d->fallback_policy_interpreter > 0 &&
                         d->fallback_translation == 0 &&
                         d->fallback_publication == 0 && d->fallback_owner == 0 &&
                         d->fallback_unavailable == 0 && d->jit_rejected == 0;
    } else if (config->mode == EXECUTOR_JIT) {
        proof.kind = "verified";
        proof.reason = "none";
        proof.accepted = d->published > 0 && d->native_executed > 0 &&
                         d->interpreter_executed == 0 && fallbacks == 0 &&
                         d->jit_rejected == 0;
    } else {
        proof.kind = "auto-active";
        proof.reason = "none";
        proof.accepted = d->published > 0 &&
                         (d->native_executed > 0 || d->interpreter_executed > 0);
    }
    if (!proof.accepted) {
        proof.kind = config->mode == EXECUTOR_JIT ? "jit-rejected" : "executor-proof-fail";
        proof.reason = config->mode == EXECUTOR_JIT ? "jit-proof-fail" :
                       "executor-proof-fail";
    }
    return proof;
}

static void emit_delta_fields(const struct bpf_delta *delta)
{
    if (!delta->available) {
        printf("published_delta=unsupported native_executed_delta=unsupported "
               "interpreter_executed_delta=unsupported "
               "fallback_policy_interpreter_delta=unsupported "
               "fallback_translation_delta=unsupported "
               "fallback_publication_delta=unsupported fallback_owner_delta=unsupported "
               "fallback_unavailable_delta=unsupported jit_rejected_delta=unsupported "
               "fallback_delta=unsupported");
        return;
    }
    const struct bpf_counters *d = &delta->values;
    printf("published_delta=%" PRIu64 " native_executed_delta=%" PRIu64
           " interpreter_executed_delta=%" PRIu64
           " fallback_policy_interpreter_delta=%" PRIu64
           " fallback_translation_delta=%" PRIu64
           " fallback_publication_delta=%" PRIu64
           " fallback_owner_delta=%" PRIu64
           " fallback_unavailable_delta=%" PRIu64
           " jit_rejected_delta=%" PRIu64 " fallback_delta=%" PRIu64,
           d->published, d->native_executed, d->interpreter_executed,
           d->fallback_policy_interpreter, d->fallback_translation,
           d->fallback_publication, d->fallback_owner, d->fallback_unavailable,
           d->jit_rejected, fallback_total(d));
}

static int clock_ns(clockid_t clock_id, uint64_t *result)
{
    struct timespec value;

    if (clock_gettime(clock_id, &value) != 0) {
        return -1;
    }
    if (value.tv_sec < 0 || value.tv_nsec < 0 || value.tv_nsec >= 1000000000L) {
        errno = EOVERFLOW;
        return -1;
    }
    *result = (uint64_t)value.tv_sec * UINT64_C(1000000000) +
              (uint64_t)value.tv_nsec;
    return 0;
}

static int install_program(const struct sock_filter *instructions,
                           size_t instruction_count)
{
    if (instruction_count > UINT16_MAX) {
        errno = E2BIG;
        return -1;
    }
    struct sock_fprog program = {
        .len = (unsigned short)instruction_count,
        .filter = (struct sock_filter *)(uintptr_t)instructions,
    };
    return (int)syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0U, &program);
}

static int set_no_new_privs(void)
{
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) {
        return -1;
    }
    return prctl(PR_GET_NO_NEW_PRIVS, 0UL, 0UL, 0UL, 0UL) == 1 ? 0 : -1;
}

static struct sock_filter bpf_stmt(unsigned short code, uint32_t value)
{
    return (struct sock_filter){.code = code, .jt = 0, .jf = 0, .k = value};
}

static struct sock_filter bpf_jump(unsigned short code, uint32_t value,
                                   unsigned char true_skip,
                                   unsigned char false_skip)
{
    return (struct sock_filter){
        .code = code,
        .jt = true_skip,
        .jf = false_skip,
        .k = value,
    };
}

static int install_short_filter(void)
{
    const struct sock_filter instructions[] = {
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, arch)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0xc000003eU, 1, 0),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
        BPF_STMT(BPF_LD | BPF_W | BPF_ABS,
                 offsetof(struct seccomp_data, nr)),
        BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (uint32_t)SYS_getppid, 0, 1),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
    };

    return install_program(instructions,
                           sizeof(instructions) / sizeof(instructions[0]));
}

static int install_256_branch_filter(void)
{
    const size_t instruction_count = 5U + BRANCH_COUNT * 2U;
    struct sock_filter *instructions =
        calloc(instruction_count, sizeof(*instructions));
    if (instructions == NULL) {
        errno = ENOMEM;
        return -1;
    }

    size_t index = 0;
    instructions[index++] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS,
                                      offsetof(struct seccomp_data, arch));
    instructions[index++] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K, 0xc000003eU,
                                      1, 0);
    instructions[index++] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS);
    instructions[index++] = bpf_stmt(BPF_LD | BPF_W | BPF_ABS,
                                      offsetof(struct seccomp_data, nr));
    for (uint32_t branch = 0; branch < BRANCH_COUNT; ++branch) {
        instructions[index++] = bpf_jump(BPF_JMP | BPF_JEQ | BPF_K,
                                          UNKNOWN_SYSCALL_BASE + branch, 0, 1);
        instructions[index++] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
    }
    instructions[index++] = bpf_stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);

    int result = install_program(instructions, index);
    free(instructions);
    return result;
}

static int install_case_filter(enum filter_case value)
{
    if (value == CASE_NO_FILTER) {
        return 0;
    }
    if (set_no_new_privs() != 0) {
        return -1;
    }
    if (value == CASE_SHORT) {
        return install_short_filter() == 0 ? 0 : -1;
    }
    if (value == CASE_BRANCH_256) {
        return install_256_branch_filter() == 0 ? 0 : -1;
    }
    for (unsigned int index = 0; index < STACKED_COUNT; ++index) {
        if (install_short_filter() != 0) {
            return -1;
        }
    }
    return 0;
}

static int target_call(uint64_t *value)
{
    errno = 0;
    long result = syscall(SYS_getppid);
    if (result <= 0) {
        return -1;
    }
    *value ^= (uint64_t)result + UINT64_C(0x9e3779b97f4a7c15);
    return 0;
}

static int correctness(uint64_t run_id, struct cell_result *result)
{
    uint64_t checksum = run_id ^ UINT64_C(0x544b50455246434f);
    for (unsigned int index = 0; index < 16U; ++index) {
        if (target_call(&checksum) != 0) {
            error_message("correctness-call");
            return -1;
        }
    }
    result->checksum = checksum;
    result->correctness_ok = true;
    return 0;
}

static int timed_call(uint64_t *wall_ns, uint64_t *cpu_ns, uint64_t *sink)
{
    uint64_t wall_start;
    uint64_t cpu_start;
    uint64_t wall_end;
    uint64_t cpu_end;

    if (clock_ns(CLOCK_MONOTONIC, &wall_start) != 0 ||
        clock_ns(CLOCK_PROCESS_CPUTIME_ID, &cpu_start) != 0 ||
        target_call(sink) != 0 ||
        clock_ns(CLOCK_PROCESS_CPUTIME_ID, &cpu_end) != 0 ||
        clock_ns(CLOCK_MONOTONIC, &wall_end) != 0) {
        error_message("latency-clock-or-call");
        return -1;
    }
    if (wall_end < wall_start || cpu_end < cpu_start) {
        errno = EPROTO;
        error_message("latency-clock-order");
        return -1;
    }
    *wall_ns = wall_end - wall_start;
    *cpu_ns = cpu_end - cpu_start;
    return 0;
}

static int compare_u64(const void *left, const void *right)
{
    const uint64_t lhs = *(const uint64_t *)left;
    const uint64_t rhs = *(const uint64_t *)right;
    return (lhs > rhs) - (lhs < rhs);
}

static uint64_t quantile(uint64_t *values, size_t count, unsigned int permille)
{
    size_t rank = (count * permille + 999U) / 1000U;
    if (rank == 0) {
        rank = 1;
    }
    if (rank > count) {
        rank = count;
    }
    qsort(values, count, sizeof(*values), compare_u64);
    return values[rank - 1U];
}

static int measure(uint64_t run_id, struct sample_set *samples, uint64_t *sink)
{
    *sink = run_id;

    for (unsigned int index = 0; index < WARMUP_SAMPLES; ++index) {
        uint64_t wall_ns;
        uint64_t cpu_ns;
        if (timed_call(&wall_ns, &cpu_ns, sink) != 0) {
            return -1;
        }
    }
    for (unsigned int index = 0; index < LATENCY_SAMPLES; ++index) {
        if (timed_call(&samples->wall_ns[index], &samples->cpu_ns[index],
                       sink) != 0) {
            return -1;
        }
    }

    return 0;
}

static void emit_correctness(enum filter_case value, uint64_t run_id,
                             const struct executor_config *config,
                             const struct cell_result *result,
                             const struct cell_proof *proof,
                             const char *status, const char *reason)
{
    printf("TKPERF_CORRECTNESS schema=%s workload=seccomp run_id=%016" PRIx64
           " cell=%s status=%s calls=%u checksum=%016" PRIx64
           " executor=%s domain=seccomp reason=%s proof=%s ",
           PERF_SCHEMA, run_id, case_name(value), status,
           result->correctness_ok ? 16U : 0U, result->checksum,
           executor_name(config->mode), reason == NULL ? "none" : reason,
           proof->kind);
    emit_delta_fields(&proof->delta);
    printf("\n");
    line_flush();
}

static void emit_window_latency(enum filter_case value, uint64_t run_id,
                                const struct executor_config *config,
                                const struct sample_set *samples,
                                uint64_t sink, bool measured)
{
    uint64_t wall_p50 = 0;
    uint64_t wall_p99 = 0;
    uint64_t cpu_p50 = 0;
    uint64_t cpu_p99 = 0;
    if (measured) {
        wall_p50 = quantile((uint64_t *)samples->wall_ns, LATENCY_SAMPLES, 500U);
        wall_p99 = quantile((uint64_t *)samples->wall_ns, LATENCY_SAMPLES, 990U);
        cpu_p50 = quantile((uint64_t *)samples->cpu_ns, LATENCY_SAMPLES, 500U);
        cpu_p99 = quantile((uint64_t *)samples->cpu_ns, LATENCY_SAMPLES, 990U);
    }
    const char *status = measured ? "ok" : "fail";
    printf("TKPERF_WINDOW schema=%s workload=seccomp run_id=%016" PRIx64
           " cell=%s status=%s warmup=%u samples=%u clocks=monotonic,process-cpu "
           "executor=%s domain=seccomp\n",
           PERF_SCHEMA, run_id, case_name(value), status, WARMUP_SAMPLES,
           measured ? LATENCY_SAMPLES : 0U, executor_name(config->mode));
    printf("TKPERF_LATENCY schema=%s workload=seccomp run_id=%016" PRIx64
           " cell=%s status=%s samples=%u wall_p50_ns=%" PRIu64
           " wall_p99_ns=%" PRIu64 " cpu_p50_ns=%" PRIu64
           " cpu_p99_ns=%" PRIu64 " sink=%016" PRIx64
           " executor=%s domain=seccomp\n",
           PERF_SCHEMA, run_id, case_name(value), status,
           measured ? LATENCY_SAMPLES : 0U, wall_p50, wall_p99, cpu_p50,
           cpu_p99, sink, executor_name(config->mode));
    line_flush();
}

static int run_child(enum filter_case value, uint64_t run_id,
                     const struct executor_config *requested)
{
    struct executor_config config = *requested;
    struct bpf_counters before;
    struct bpf_counters after;
    struct bpf_delta delta = {.available = false, .valid = false};
    struct cell_result result = {.checksum = 0, .correctness_ok = false};
    struct sample_set samples;
    uint64_t sink = run_id;
    int control_state = set_executor_control("seccomp", config.mode);
    if (control_state < 0 ||
        (config.mode != EXECUTOR_AUTO && control_state != 1)) {
        struct cell_proof proof = {.delta = delta, .kind = "unsupported-ablation",
                                   .reason = "bpf-control-unavailable", .accepted = false};
        emit_correctness(value, run_id, &config, &result, &proof, "unsupported",
                         proof.reason);
        return 2;
    }
    int before_state = read_bpf_stats("seccomp", &before);
    if (before_state < 0 ||
        (config.mode != EXECUTOR_AUTO && before_state != 1)) {
        struct cell_proof proof = {.delta = delta, .kind = "unsupported-ablation",
                                   .reason = "bpf-stats-unavailable", .accepted = false};
        emit_correctness(value, run_id, &config, &result, &proof, "unsupported",
                         proof.reason);
        return 2;
    }
    bool installed = install_case_filter(value) == 0;
    if (!installed) {
        error_message("install-filter");
    } else if (correctness(run_id, &result) != 0) {
        result.correctness_ok = false;
    }
    bool measured = false;
    if (installed && result.correctness_ok) {
        measured = measure(run_id, &samples, &sink) == 0;
        if (!measured) {
            error_message("latency-measure");
        }
    }
    int after_state = read_bpf_stats("seccomp", &after);
    if (before_state == 1 && after_state == 1) {
        (void)make_delta(&before, &after, &delta);
    }
    struct cell_proof proof = evaluate_proof(value, &config, &delta,
                                              result.correctness_ok);
    const char *status = proof.accepted ? "ok" :
        (config.mode == EXECUTOR_AUTO && !delta.available && result.correctness_ok ? "ok" :
         (proof.kind[0] == 'u' || proof.kind[0] == 'j' ? "unsupported" : "fail"));
    const char *reason = proof.accepted ? "none" : proof.reason;
    emit_correctness(value, run_id, &config, &result, &proof, status, reason);
    if (!installed || !result.correctness_ok || !proof.accepted) {
        return proof.accepted ? 1 : 2;
    }
    emit_window_latency(value, run_id, &config, &samples, sink, measured);
    return measured ? 0 : 1;
}

static int run_isolated(enum filter_case value, uint64_t run_id,
                        const struct executor_config *config)
{
    pid_t child = fork();
    if (child < 0) {
        error_message("fork");
        return -1;
    }
    if (child == 0) {
        _exit(run_child(value, run_id, config));
    }
    int status = 0;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status)) {
        errno = ECHILD;
        error_message("child-cell");
        return -1;
    }
    if (WEXITSTATUS(status) == 0) {
        return 0;
    }
    if (WEXITSTATUS(status) == 2) {
        return 2;
    }
    errno = ECHILD;
    error_message("child-cell");
    return 1;
}

static uint64_t make_run_id(void)
{
    uint64_t now = 0;
    if (clock_ns(CLOCK_MONOTONIC, &now) != 0) {
        now = UINT64_C(0x544b504552464155);
    }
    return now ^ ((uint64_t)(uint32_t)getpid() << 32);
}

int main(int argc, char **argv)
{
    enum executor_mode executor = EXECUTOR_AUTO;
    if (argc > 1) {
        const char *value = NULL;
        if (argc == 3 && strcmp(argv[1], "--executor") == 0) {
            value = argv[2];
        } else if (argc == 2 && strncmp(argv[1], "--executor=", 11) == 0) {
            value = argv[1] + 11;
        }
        if (value == NULL || parse_executor(value, &executor) != 0) {
            errno = EINVAL;
            error_message("arguments");
            fprintf(stderr, "usage: seccomp-perf [--executor auto|interpreter|jit]\n");
            return EXIT_FAILURE;
        }
    }
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    uint64_t run_id = make_run_id();
    printf("TKPERF_RUN schema=%s workload=seccomp run_id=%016" PRIx64
           " cells=%u clocks=monotonic,process-cpu executor=%s domain=seccomp\n",
           PERF_SCHEMA, run_id, CASE_COUNT, executor_name(executor));
    line_flush();
    struct executor_config config;
    int capability = prepare_executor("seccomp", executor, &config);
    unsigned int unsupported = 0;
    bool failed = false;
    if (capability <= 0) {
        for (enum filter_case value = CASE_NO_FILTER; value < CASE_COUNT; ++value) {
            struct bpf_delta delta = {.available = false, .valid = false};
            struct cell_proof proof = {.delta = delta, .kind = "unsupported-ablation",
                                       .reason = capability < 0 ? "bpf-proc-error" :
                                                 "bpf-control-unavailable",
                                       .accepted = false};
            struct cell_result result = {.checksum = 0, .correctness_ok = false};
            emit_correctness(value, run_id, &config, &result, &proof,
                             "unsupported", proof.reason);
            ++unsupported;
        }
    } else {
    for (enum filter_case value = CASE_NO_FILTER; value < CASE_COUNT; ++value) {
        int result = run_isolated(value, run_id, &config);
        if (result == 2) {
            ++unsupported;
        } else if (result != 0) {
            failed = true;
        }
    }
    }
    printf("TKPERF_DONE schema=%s workload=seccomp run_id=%016" PRIx64
           " status=%s cells=%u unsupported=%u executor=%s domain=seccomp proof=%s\n",
           PERF_SCHEMA, run_id, failed ? "fail" :
           (unsupported != 0 ? "unsupported" : "ok"), CASE_COUNT,
           unsupported, executor_name(executor), failed ? "fail" :
           (unsupported != 0 ? "unsupported" : "verified"));
    line_flush();
    return failed ? EXIT_FAILURE : EXIT_SUCCESS;
}
