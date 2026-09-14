#define _GNU_SOURCE

/* TheKernel guest-toolchain Phase 0: what shape does the synthesized procfs
 * have, and where does the kernel place mappings?
 *
 * A C compiler and a TCG system emulator are ordinary userspace processes
 * that were written against Linux's synthesized /proc and against a Linux
 * address-space layout.  Before porting them we need to separate "the
 * workload really needs this" from "we assumed it needs this".  Every
 * observation below is therefore classified as either:
 *
 *   REQUIRED       the workload cannot start without it.  A failure is a
 *                  kernel bug or a deliberately missing feature, the probe
 *                  prints THEKERNEL_PROC_SHAPE_FAIL and exits non-zero.
 *   INFORMATIONAL  the workload only prefers it, or can fall back to another
 *                  source.  The value is printed for the decision record and
 *                  never fails the probe, even when it looks broken.
 *
 * The probe keeps running after a REQUIRED failure in one section so a single
 * guest run reports as many independent findings as possible; the exit status
 * is still EXIT_FAILURE.  Failures never abort on data the probe only reads
 * informationally.
 *
 * Stable markers:
 *   THEKERNEL_PROC_SHAPE_OK
 *   THEKERNEL_PROC_SHAPE_FAIL <operation> k=v ... errno=<n> (<message>)
 *   THEKERNEL_PROC_SHAPE_INFO <k=v ...>          (greppable observations)
 */

#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <unistd.h>

#if !defined(__x86_64__)
#error "the proc shape smoke is x86_64-only"
#endif

/* Forward declaration so the address-placement probe can report the code
 * anchor the same way /proc/self/maps is asked about it. */
int main(void);

/* These are Linux x86_64 ABI values, not libc-private declarations.  glibc
 * happens to expose MAP_FIXED_NOREPLACE through <sys/mman.h>, but musl and a
 * reduced guest rootfs header set may not, so keep the value local.  A kernel
 * that predates the flag silently ignores it, which changes what mmap()
 * returns; the probe must be able to send the flag either way. */
#ifndef MAP_FIXED_NOREPLACE
#define MAP_FIXED_NOREPLACE 0x100000
#endif

/* ELF auxiliary vector types from the Linux x86_64 ABI (<linux/auxvec.h>,
 * also mirrored in <elf.h>).  Defined locally so the probe does not depend on
 * either header being shipped in the guest rootfs. */
#define TK_AT_NULL 0
#define TK_AT_PHDR 3
#define TK_AT_PAGESZ 6
#define TK_AT_ENTRY 9
#define TK_AT_UID 11
#define TK_AT_HWCAP 16
#define TK_AT_RANDOM 25
#define TK_AT_HWCAP2 26
#define TK_AT_EXECFN 31

#define ANON_PROBE_PAGES 3U
#define HINT_PROBE_PAGES 4U
#define FIXED_PROBE_PAGES 4U
#define AUXV_PAIR_BYTES 16U

/* One shared BSS buffer keeps the probe's own memory use off the (possibly
 * small) guest main-thread stack, and makes the address-space observations
 * the probe reports easy to attribute. */
#define READ_BUFFER_CAPACITY (1024U * 1024U)
#define MAPS_CAPACITY READ_BUFFER_CAPACITY
#define CPUINFO_CAPACITY READ_BUFFER_CAPACITY
#define STATUS_CAPACITY (64U * 1024U)
#define SELF_FILE_CAPACITY (64U * 1024U)
#define EXE_READ_CAPACITY 64U

#define STATUS_MAX_FIELDS 128U
#define STATUS_KEY_MAX 64U
#define STATUS_VALUE_MAX 256U
#define STATUS_EXTRA_PRINT_LIMIT 32U

#define SELF_LINK_CAPACITY 4096U

enum self_file_shape {
    SELF_FILE_BINARY = 0,
    SELF_FILE_NUL_SEPARATED = 1,
};

struct file_bytes {
    size_t length;
    int truncated;
};

struct maps_expectation {
    const void *code;
    const void *anon_base;
    size_t anon_bytes;
};

struct maps_summary {
    size_t lines;
    size_t exec_lines;
    size_t writable_lines;
    size_t named_lines;
    int code_in_exec;
    int anon_covered;
    int anon_writable;
    int anon_anonymous;
    char anon_perms[5];
    size_t anon_path_length;
    char anon_path[STATUS_KEY_MAX];
};

struct status_field {
    const char *key;
    size_t key_length;
    const char *value;
    size_t value_length;
};

struct cpuinfo_summary {
    size_t lines;
    size_t processors;
    size_t flags_fields;
    size_t first_flags_count;
    size_t flag_tokens;
};

static const char *const cpuinfo_dispatch_flags[] = {
    "sse2", "sse3",    "ssse3", "sse4_1", "sse4_2", "avx",
    "avx2", "fpu",     "tsc",   "cx16",   "lm",     "pdpe1gb",
    "rdrand", "rdseed", "fsgsbase", "xsave", "osxsave",
};

#define CPUINFO_DISPATCH_FLAG_COUNT \
    (sizeof(cpuinfo_dispatch_flags) / sizeof(cpuinfo_dispatch_flags[0]))

static char read_buffer[READ_BUFFER_CAPACITY + 1U];
static char link_buffer[SELF_LINK_CAPACITY];

static size_t page_bytes;
static void *anon_region_base;
static size_t anon_region_total_bytes;
static int required_failures;

/* Informational line.  Never fails the probe, never sets the exit status. */
static void __attribute__((format(printf, 1, 2)))
info(const char *format, ...) {
    va_list arguments;
    fputs("THEKERNEL_PROC_SHAPE_INFO ", stdout);
    va_start(arguments, format);
    vprintf(format, arguments);
    va_end(arguments);
    fputc('\n', stdout);
}

/* Required failure.  Prints the operation, the k=v evidence for the semantic
 * condition, and the errno/strerror pair the house conventions ask for. */
static int __attribute__((format(printf, 2, 3)))
failf(const char *operation, const char *format, ...) {
    const int saved_errno = errno;
    va_list arguments;
    fprintf(stderr, "THEKERNEL_PROC_SHAPE_FAIL %s ", operation);
    va_start(arguments, format);
    vfprintf(stderr, format, arguments);
    va_end(arguments);
    fprintf(stderr, " errno=%d (%s)\n", saved_errno, strerror(saved_errno));
    required_failures += 1;
    return 1;
}

/* Reads at most `capacity` bytes into the shared buffer, NUL-terminating after
 * the data so line-oriented scanners can use it without a length.  Returns 0
 * on success, -1 with errno set when the path cannot be opened or read. */
static int read_path_bounded(const char *path, size_t capacity,
                             struct file_bytes *bytes) {
    if (capacity > READ_BUFFER_CAPACITY) {
        errno = EOVERFLOW;
        return -1;
    }
    bytes->length = 0;
    bytes->truncated = 0;
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return -1;
    }
    size_t used = 0;
    for (;;) {
        if (used == capacity) {
            /* The file may or may not have more data; callers treat a full
             * buffer as "unbounded content" rather than as a kernel bug. */
            bytes->truncated = 1;
            break;
        }
        ssize_t count = read(fd, read_buffer + used, capacity - used);
        if (count < 0) {
            const int saved_errno = errno;
            (void)close(fd);
            errno = saved_errno;
            return -1;
        }
        if (count == 0) {
            break;
        }
        used += (size_t)count;
    }
    if (close(fd) != 0) {
        return -1;
    }
    read_buffer[used] = '\0';
    bytes->length = used;
    return 0;
}

static uint64_t load_u64_native(const unsigned char *bytes) {
    uint64_t value = 0;
    /* memcpy, not a cast: /proc data carries no alignment guarantee.  x86_64
     * is little-endian, which is what the auxv pair layout assumes. */
    memcpy(&value, bytes, sizeof(value));
    return value;
}

static int parse_hex_uintptr(const char **cursor, const char *limit,
                             uintptr_t *value) {
    const char *position = *cursor;
    uintptr_t result = 0;
    size_t digits = 0;
    while (position < limit) {
        unsigned digit;
        if (*position >= '0' && *position <= '9') {
            digit = (unsigned)(*position - '0');
        } else if (*position >= 'a' && *position <= 'f') {
            digit = (unsigned)(*position - 'a') + 10U;
        } else if (*position >= 'A' && *position <= 'F') {
            digit = (unsigned)(*position - 'A') + 10U;
        } else {
            break;
        }
        if (result > (UINTPTR_MAX >> 4)) {
            return -1;
        }
        result = (result << 4) | (uintptr_t)digit;
        position += 1;
        digits += 1;
    }
    if (digits == 0) {
        return -1;
    }
    *cursor = position;
    *value = result;
    return 0;
}

static int maps_perms_valid(const char perms[4]) {
    if (perms[0] != 'r' && perms[0] != '-') {
        return 0;
    }
    if (perms[1] != 'w' && perms[1] != '-') {
        return 0;
    }
    if (perms[2] != 'x' && perms[2] != '-') {
        return 0;
    }
    if (perms[3] != 'p' && perms[3] != 's') {
        return 0;
    }
    return 1;
}

/* Anonymous mappings must not claim a file-backed pathname.  Linux prints
 * either nothing or a bracketed pseudo-name such as [anon] / [anon:name];
 * a compiler that scans maps to size its heap relies on that distinction. */
static int maps_path_is_anonymous(const char *path, size_t length) {
    if (length == 0) {
        return 1;
    }
    if (length >= 6U && memcmp(path, "[anon]", 6U) == 0) {
        return 1;
    }
    if (length >= 6U && memcmp(path, "[anon:", 6U) == 0) {
        return 1;
    }
    return 0;
}

static unsigned char anonymous_pattern_value(size_t page_index) {
    return (unsigned char)(0xa5U ^ (unsigned)(page_index * 37U));
}

/* Touch both ends of every page: a kernel that rounds a mapping's length down
 * or forgets the final page is caught by the read-back, not by the mmap
 * return value alone. */
static void fill_anonymous_pattern(void *mapping, size_t pages) {
    volatile unsigned char *bytes = mapping;
    for (size_t page = 0; page < pages; ++page) {
        bytes[page * page_bytes] = anonymous_pattern_value(page);
        bytes[(page + 1U) * page_bytes - 1U] =
            (unsigned char)(anonymous_pattern_value(page) ^ 0xffU);
    }
}

static int anonymous_pattern_intact(const void *mapping, size_t pages) {
    const volatile unsigned char *bytes = mapping;
    for (size_t page = 0; page < pages; ++page) {
        if (bytes[page * page_bytes] != anonymous_pattern_value(page)) {
            return 0;
        }
        if (bytes[(page + 1U) * page_bytes - 1U] !=
            (unsigned char)(anonymous_pattern_value(page) ^ 0xffU)) {
            return 0;
        }
    }
    return 1;
}

static int create_anonymous_probe_region(void) {
    const size_t bytes = ANON_PROBE_PAGES * page_bytes;
    void *mapping = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) {
        return failf("mmap-anonymous-probe-region", "bytes=%zu", bytes);
    }
    anon_region_base = mapping;
    anon_region_total_bytes = bytes;
    fill_anonymous_pattern(mapping, ANON_PROBE_PAGES);
    if (!anonymous_pattern_intact(mapping, ANON_PROBE_PAGES)) {
        errno = EIO;
        return failf("mmap-anonymous-probe-region",
                     "bytes=%zu address=%p reason=readback-mismatch", bytes,
                     mapping);
    }
    return 0;
}

static void release_anonymous_probe_region(void) {
    if (anon_region_base != NULL) {
        (void)munmap(anon_region_base, anon_region_total_bytes);
        anon_region_base = NULL;
    }
}

/* REQUIRED 1: /proc/self/maps is parseable and self-consistent.
 *
 * A compiler's driver, a linker, and QEMU's TCG all walk this file to learn
 * where their own text, heap, and reserved regions live.  The required
 * contract is structural, not numeric: addresses parse and ascend, one
 * executable region contains the running code, a mapping we just created is
 * visible there, and the text is newline-terminated.  The number of lines is
 * host- and kernel-dependent and is only reported. */
static int check_proc_self_maps(const struct maps_expectation *expect) {
    struct file_bytes bytes;
    if (read_path_bounded("/proc/self/maps", MAPS_CAPACITY, &bytes) != 0) {
        return failf("maps-open", "path=/proc/self/maps");
    }
    char *data = read_buffer;
    size_t length = bytes.length;
    if (bytes.truncated != 0) {
        /* Drop the trailing partial line: it is an artifact of our buffer,
         * not a property of the file. */
        size_t cut = length;
        while (cut > 0 && data[cut - 1U] != '\n') {
            cut -= 1U;
        }
        length = cut;
    }
    if (length == 0) {
        errno = ENODATA;
        return failf("maps-empty", "path=/proc/self/maps bytes=%zu truncated=%d",
                     bytes.length, bytes.truncated);
    }
    if (bytes.truncated == 0 && data[length - 1U] != '\n') {
        errno = EPROTO;
        return failf("maps-not-newline-terminated", "bytes=%zu last_byte=0x%02x",
                     length, (unsigned)(unsigned char)data[length - 1U]);
    }

    struct maps_summary summary;
    memset(&summary, 0, sizeof(summary));
    const uintptr_t code_address = (uintptr_t)expect->code;
    const uintptr_t anon_begin = (uintptr_t)expect->anon_base;
    const uintptr_t anon_end = anon_begin + expect->anon_bytes;

    size_t cursor = 0;
    size_t line_number = 0;
    uintptr_t previous_end = 0;
    int have_previous = 0;
    while (cursor < length) {
        const size_t line_start = cursor;
        while (cursor < length && data[cursor] != '\n') {
            cursor += 1U;
        }
        const size_t line_end = cursor;
        if (cursor < length) {
            cursor += 1U;
        }
        line_number += 1U;

        const char *position = data + line_start;
        const char *limit = data + line_end;
        uintptr_t start = 0;
        uintptr_t end = 0;
        if (parse_hex_uintptr(&position, limit, &start) != 0 || position >= limit ||
            *position != '-') {
            errno = EPROTO;
            return failf("maps-address-range", "line=%zu reason=start-not-hex",
                         line_number);
        }
        position += 1;
        if (parse_hex_uintptr(&position, limit, &end) != 0) {
            errno = EPROTO;
            return failf("maps-address-range", "line=%zu reason=end-not-hex",
                         line_number);
        }
        if (start >= end) {
            errno = EPROTO;
            return failf("maps-address-range",
                         "line=%zu reason=range-not-ordered start=0x%" PRIxPTR
                         " end=0x%" PRIxPTR,
                         line_number, start, end);
        }
        if (position >= limit || *position != ' ') {
            errno = EPROTO;
            return failf("maps-perms", "line=%zu reason=missing-perms-field",
                         line_number);
        }
        position += 1;
        if ((size_t)(limit - position) < 4U) {
            errno = EPROTO;
            return failf("maps-perms", "line=%zu reason=short-perms-field",
                         line_number);
        }
        char perms[5];
        memcpy(perms, position, 4U);
        perms[4] = '\0';
        if (!maps_perms_valid(perms)) {
            errno = EPROTO;
            return failf("maps-perms", "line=%zu perms=%s", line_number, perms);
        }
        position += 4;

        /* offset, device, and inode are part of the Linux format but are not
         * what this probe asserts; skip their tokens to reach the pathname,
         * which may be empty or may itself contain spaces. */
        for (int field = 0; field < 3; ++field) {
            while (position < limit && (*position == ' ' || *position == '\t')) {
                position += 1;
            }
            while (position < limit && *position != ' ' && *position != '\t') {
                position += 1;
            }
        }
        while (position < limit && (*position == ' ' || *position == '\t')) {
            position += 1;
        }
        const char *path = position;
        size_t path_length = (size_t)(limit - path);
        while (path_length > 0 &&
               (path[path_length - 1U] == ' ' || path[path_length - 1U] == '\t' ||
                path[path_length - 1U] == '\r')) {
            path_length -= 1U;
        }

        if (have_previous != 0 && start < previous_end) {
            errno = EPROTO;
            return failf("maps-ordering",
                         "line=%zu start=0x%" PRIxPTR " previous_end=0x%" PRIxPTR,
                         line_number, start, previous_end);
        }
        previous_end = end;
        have_previous = 1;

        summary.lines += 1U;
        if (perms[1] == 'w') {
            summary.writable_lines += 1U;
        }
        if (path_length > 0) {
            summary.named_lines += 1U;
        }
        if (perms[2] == 'x') {
            summary.exec_lines += 1U;
            if (start <= code_address && code_address < end) {
                summary.code_in_exec = 1;
            }
        }
        if (start <= anon_begin && anon_end <= end) {
            summary.anon_covered = 1;
            summary.anon_writable = (perms[0] == 'r' && perms[1] == 'w');
            summary.anon_anonymous = maps_path_is_anonymous(path, path_length);
            memcpy(summary.anon_perms, perms, sizeof(summary.anon_perms));
            summary.anon_path_length =
                path_length < sizeof(summary.anon_path) ? path_length
                                                        : sizeof(summary.anon_path);
            memcpy(summary.anon_path, path, summary.anon_path_length);
        }
    }

    info("maps lines=%zu exec_lines=%zu writable_lines=%zu named_lines=%zu "
         "truncated=%d",
         summary.lines, summary.exec_lines, summary.writable_lines,
         summary.named_lines, bytes.truncated);
    info("maps_code main=%p exec_lines=%zu in_exec=%d", expect->code,
         summary.exec_lines, summary.code_in_exec);
    info("maps_anon_region base=%p bytes=%zu covered=%d anonymous=%d "
         "writable=%d perms=%s path=%.*s",
         expect->anon_base, expect->anon_bytes, summary.anon_covered,
         summary.anon_anonymous, summary.anon_writable, summary.anon_perms,
         (int)summary.anon_path_length, summary.anon_path);

    if (summary.exec_lines == 0) {
        errno = EPROTO;
        return failf("maps-no-exec-region", "lines=%zu exec_lines=0",
                     summary.lines);
    }
    if (summary.code_in_exec == 0) {
        errno = EPROTO;
        return failf("maps-code-not-in-exec-region",
                     "main=%p exec_lines=%zu lines=%zu", expect->code,
                     summary.exec_lines, summary.lines);
    }
    if (summary.anon_covered == 0) {
        errno = EPROTO;
        return failf("maps-anon-region-missing",
                     "base=%p bytes=%zu lines=%zu truncated=%d",
                     expect->anon_base, expect->anon_bytes, summary.lines,
                     bytes.truncated);
    }
    if (summary.anon_anonymous == 0) {
        errno = EPROTO;
        return failf("maps-anon-region-named", "base=%p path=%.*s",
                     expect->anon_base, (int)summary.anon_path_length,
                     summary.anon_path);
    }
    if (summary.anon_writable == 0) {
        errno = EPROTO;
        return failf("maps-anon-region-not-writable", "base=%p perms=%s",
                     expect->anon_base, summary.anon_perms);
    }
    if (!anonymous_pattern_intact(expect->anon_base, ANON_PROBE_PAGES)) {
        errno = EIO;
        return failf("maps-anon-region-contents", "base=%p bytes=%zu",
                     expect->anon_base, expect->anon_bytes);
    }

    puts("THEKERNEL_PROC_SHAPE_MAPS_OK");
    return 0;
}

static void parse_status_fields(const char *data, size_t length,
                                struct status_field *fields, size_t capacity,
                                size_t *stored, size_t *total, size_t *lines) {
    size_t cursor = 0;
    while (cursor < length) {
        const size_t line_start = cursor;
        while (cursor < length && data[cursor] != '\n') {
            cursor += 1U;
        }
        const size_t line_end = cursor;
        if (cursor < length) {
            cursor += 1U;
        }
        *lines += 1U;
        const char *line = data + line_start;
        const size_t line_length = line_end - line_start;
        size_t begin = 0;
        while (begin < line_length &&
               (line[begin] == ' ' || line[begin] == '\t')) {
            begin += 1U;
        }
        size_t colon = begin;
        while (colon < line_length && line[colon] != ':') {
            colon += 1U;
        }
        if (colon >= line_length) {
            continue;
        }
        size_t key_end = colon;
        while (key_end > begin &&
               (line[key_end - 1U] == ' ' || line[key_end - 1U] == '\t')) {
            key_end -= 1U;
        }
        if (key_end == begin) {
            continue;
        }
        size_t value_begin = colon + 1U;
        while (value_begin < line_length &&
               (line[value_begin] == ' ' || line[value_begin] == '\t')) {
            value_begin += 1U;
        }
        size_t value_end = line_length;
        while (value_end > value_begin &&
               (line[value_end - 1U] == ' ' || line[value_end - 1U] == '\t' ||
                line[value_end - 1U] == '\r')) {
            value_end -= 1U;
        }
        *total += 1U;
        if (*stored >= capacity) {
            continue;
        }
        struct status_field *field = &fields[*stored];
        size_t key_length = key_end - begin;
        size_t value_length = value_end - value_begin;
        field->key = line + begin;
        field->key_length = key_length < STATUS_KEY_MAX ? key_length
                                                        : STATUS_KEY_MAX;
        field->value = line + value_begin;
        field->value_length = value_length < STATUS_VALUE_MAX
                                  ? value_length
                                  : STATUS_VALUE_MAX;
        *stored += 1U;
    }
}

static const struct status_field *status_lookup(
    const struct status_field *fields, size_t count, const char *key) {
    const size_t key_length = strlen(key);
    for (size_t index = 0; index < count; ++index) {
        if (fields[index].key_length == key_length &&
            memcmp(fields[index].key, key, key_length) == 0) {
            return &fields[index];
        }
    }
    return NULL;
}

static int parse_uint_prefix(const char *text, size_t length, uint64_t *value,
                             size_t *consumed) {
    size_t index = 0;
    uint64_t result = 0;
    while (index < length && text[index] >= '0' && text[index] <= '9') {
        const uint64_t digit = (uint64_t)(text[index] - '0');
        if (result > (UINT64_MAX - digit) / 10U) {
            return -1;
        }
        result = result * 10U + digit;
        index += 1U;
    }
    if (index == 0) {
        return -1;
    }
    *value = result;
    *consumed = index;
    return 0;
}

static int status_require_text(const struct status_field *fields, size_t count,
                               const char *key) {
    const struct status_field *field = status_lookup(fields, count, key);
    if (field == NULL) {
        errno = EPROTO;
        return failf("status-field", "key=%s reason=missing", key);
    }
    if (field->value_length == 0) {
        errno = EPROTO;
        return failf("status-field", "key=%s reason=empty-value", key);
    }
    return 0;
}

/* Uid and Gid carry four numbers per line; the runtime only needs the first
 * (real) identity to parse, so trailing tab-separated numbers are allowed. */
static int status_require_number(const struct status_field *fields,
                                 size_t count, const char *key,
                                 uint64_t *value) {
    const struct status_field *field = status_lookup(fields, count, key);
    if (field == NULL) {
        errno = EPROTO;
        return failf("status-field", "key=%s reason=missing", key);
    }
    size_t consumed = 0;
    if (parse_uint_prefix(field->value, field->value_length, value, &consumed) !=
        0) {
        errno = EPROTO;
        return failf("status-field", "key=%s reason=not-a-number value=%.*s", key,
                     (int)field->value_length, field->value);
    }
    if (consumed < field->value_length && field->value[consumed] != ' ' &&
        field->value[consumed] != '\t') {
        errno = EPROTO;
        return failf("status-field", "key=%s reason=trailing-garbage value=%.*s",
                     key, (int)field->value_length, field->value);
    }
    return 0;
}

static int status_is_required_key(const struct status_field *field) {
    static const char *const required[] = {
        "Name", "Pid", "PPid", "Uid", "Gid", "Threads", "VmSize", "VmRSS",
    };
    for (size_t index = 0; index < sizeof(required) / sizeof(required[0]);
         ++index) {
        const size_t length = strlen(required[index]);
        if (field->key_length == length &&
            memcmp(field->key, required[index], length) == 0) {
            return 1;
        }
    }
    return 0;
}

/* REQUIRED 2: /proc/self/status answers the questions a runtime asks about
 * itself.
 *
 * A compiler driver, a thread pool, and QEMU's memory accounting all read
 * these keys; a missing key makes a Linux program fall back to a syscall or,
 * worse, to a wrong default.  Only presence and parseability are required --
 * the magnitudes are environment-dependent and are reported, not asserted. */
static int check_proc_self_status(void) {
    struct file_bytes bytes;
    if (read_path_bounded("/proc/self/status", STATUS_CAPACITY, &bytes) != 0) {
        return failf("status-open", "path=/proc/self/status");
    }
    static struct status_field fields[STATUS_MAX_FIELDS];
    size_t stored = 0;
    size_t total = 0;
    size_t lines = 0;
    parse_status_fields(read_buffer, bytes.length, fields, STATUS_MAX_FIELDS,
                        &stored, &total, &lines);

    info("status lines=%zu fields=%zu fields_kept=%zu truncated=%d", lines,
         total, stored, bytes.truncated);

    size_t extra_total = 0;
    size_t extra_printed = 0;
    fputs("THEKERNEL_PROC_SHAPE_INFO status_extra_keys=", stdout);
    for (size_t index = 0; index < stored; ++index) {
        if (status_is_required_key(&fields[index]) != 0) {
            continue;
        }
        extra_total += 1U;
        if (extra_printed >= STATUS_EXTRA_PRINT_LIMIT) {
            continue;
        }
        printf("%s%.*s", extra_printed == 0 ? "" : ",",
               (int)fields[index].key_length, fields[index].key);
        extra_printed += 1U;
    }
    printf(" status_extra_count=%zu status_extra_printed=%zu\n", extra_total,
           extra_printed);

    int result = 0;
    const struct status_field *name = status_lookup(fields, stored, "Name");
    if (status_require_text(fields, stored, "Name") != 0) {
        result = 1;
    }
    uint64_t pid = 0;
    uint64_t ppid = 0;
    uint64_t uid = 0;
    uint64_t gid = 0;
    uint64_t threads = 0;
    uint64_t vm_size = 0;
    uint64_t vm_rss = 0;
    /* Deliberately not short-circuited: one guest run should list every field
     * the kernel is missing, not only the first one. */
    if (status_require_number(fields, stored, "Pid", &pid) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "PPid", &ppid) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "Uid", &uid) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "Gid", &gid) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "Threads", &threads) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "VmSize", &vm_size) != 0) {
        result = 1;
    }
    if (status_require_number(fields, stored, "VmRSS", &vm_rss) != 0) {
        result = 1;
    }
    if (result != 0) {
        return result;
    }

    info("status_values Name=%.*s Pid=%" PRIu64 " PPid=%" PRIu64 " Uid=%" PRIu64
         " Gid=%" PRIu64 " Threads=%" PRIu64 " VmSize=%" PRIu64
         " VmRSS=%" PRIu64 " (Vm values in kB)",
         (int)name->value_length, name->value, pid, ppid, uid, gid, threads,
         vm_size, vm_rss);
    puts("THEKERNEL_PROC_SHAPE_STATUS_OK");
    return 0;
}

static void cpuinfo_scan_flags(const char *text, size_t length,
                               unsigned char *present, size_t *token_total) {
    size_t index = 0;
    while (index < length) {
        while (index < length && (text[index] == ' ' || text[index] == '\t')) {
            index += 1U;
        }
        const size_t start = index;
        while (index < length && text[index] != ' ' && text[index] != '\t') {
            index += 1U;
        }
        const size_t token_length = index - start;
        if (token_length == 0) {
            continue;
        }
        *token_total += 1U;
        for (size_t flag = 0; flag < CPUINFO_DISPATCH_FLAG_COUNT; ++flag) {
            const char *name = cpuinfo_dispatch_flags[flag];
            if (strlen(name) == token_length &&
                memcmp(name, text + start, token_length) == 0) {
                present[flag] = 1;
            }
        }
    }
}

static void parse_cpuinfo(const char *data, size_t length,
                          struct cpuinfo_summary *summary,
                          unsigned char *present) {
    size_t cursor = 0;
    int in_flags = 0;
    while (cursor < length) {
        const size_t line_start = cursor;
        while (cursor < length && data[cursor] != '\n') {
            cursor += 1U;
        }
        const size_t line_end = cursor;
        if (cursor < length) {
            cursor += 1U;
        }
        const char *line = data + line_start;
        const size_t line_length = line_end - line_start;
        size_t colon = 0;
        while (colon < line_length && line[colon] != ':') {
            colon += 1U;
        }
        if (colon >= line_length) {
            /* A wrapped feature list continues on a line with no colon.  Linux
             * x86 keeps flags on one line, but a synthesized cpuinfo may not,
             * so fold the continuation into the preceding flags field. */
            if (in_flags != 0) {
                cpuinfo_scan_flags(line, line_length, present,
                                   &summary->flag_tokens);
            }
            continue;
        }
        size_t key_end = colon;
        while (key_end > 0 &&
               (line[key_end - 1U] == ' ' || line[key_end - 1U] == '\t')) {
            key_end -= 1U;
        }
        size_t key_start = 0;
        while (key_start < key_end &&
               (line[key_start] == ' ' || line[key_start] == '\t')) {
            key_start += 1U;
        }
        const size_t key_length = key_end - key_start;
        summary->lines += 1U;
        if (key_length == 9U && memcmp(line + key_start, "processor", 9U) == 0) {
            summary->processors += 1U;
            in_flags = 0;
        } else if (key_length == 5U &&
                   memcmp(line + key_start, "flags", 5U) == 0) {
            const size_t value_start = colon + 1U;
            const size_t before = summary->flag_tokens;
            cpuinfo_scan_flags(line + value_start, line_length - value_start,
                               present, &summary->flag_tokens);
            summary->flags_fields += 1U;
            if (summary->flags_fields == 1U) {
                summary->first_flags_count = summary->flag_tokens - before;
            }
            in_flags = 1;
        } else {
            in_flags = 0;
        }
    }
}

/* REQUIRED 3: /proc/cpuinfo has at least one processor record with a flags
 * field.
 *
 * A JIT and a compiler dispatch on this text the same way they dispatch on
 * AT_HWCAP: no processor record means "I cannot tell what I am running on",
 * and an empty flags list is worse than an absent one because it silently
 * selects the baseline path.  Which flags are present is a host property and
 * is reported for the record instead of asserted. */
static int check_proc_cpuinfo(void) {
    struct file_bytes bytes;
    if (read_path_bounded("/proc/cpuinfo", CPUINFO_CAPACITY, &bytes) != 0) {
        return failf("cpuinfo-open", "path=/proc/cpuinfo");
    }
    struct cpuinfo_summary summary;
    memset(&summary, 0, sizeof(summary));
    unsigned char present[CPUINFO_DISPATCH_FLAG_COUNT];
    memset(present, 0, sizeof(present));
    parse_cpuinfo(read_buffer, bytes.length, &summary, present);

    info("cpuinfo lines=%zu processors=%zu flags_fields=%zu first_flags=%zu "
         "flag_tokens=%zu truncated=%d",
         summary.lines, summary.processors, summary.flags_fields,
         summary.first_flags_count, summary.flag_tokens, bytes.truncated);
    fputs("THEKERNEL_PROC_SHAPE_INFO cpuinfo_dispatch_flags", stdout);
    for (size_t flag = 0; flag < CPUINFO_DISPATCH_FLAG_COUNT; ++flag) {
        printf(" %s=%d", cpuinfo_dispatch_flags[flag], present[flag] ? 1 : 0);
    }
    fputc('\n', stdout);

    int result = 0;
    if (summary.processors == 0) {
        errno = EPROTO;
        result = failf("cpuinfo-no-processor", "lines=%zu processors=0",
                       summary.lines);
    }
    if (summary.flags_fields == 0) {
        errno = EPROTO;
        result = failf("cpuinfo-no-flags-field",
                       "lines=%zu processors=%zu flags_fields=0",
                       summary.lines, summary.processors);
    } else if (summary.flag_tokens == 0) {
        errno = EPROTO;
        result = failf("cpuinfo-empty-flags-field", "flags_fields=%zu",
                       summary.flags_fields);
    }
    if (result != 0) {
        return result;
    }
    puts("THEKERNEL_PROC_SHAPE_CPUINFO_OK");
    return 0;
}

/* INFORMATIONAL 4: /proc/self/auxv.
 *
 * glibc's startup and several JITs read the auxiliary vector through the
 * kernel-provided file descriptor when they cannot use the stack copy.  A
 * kernel without it is survivable, so absence is reported and never fails.
 * A truncated or unterminated vector is reported loudly because it means the
 * file exists but lies. */
static void report_proc_self_auxv(void) {
    static unsigned char buffer[4096];
    int fd = open("/proc/self/auxv", O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        const int saved_errno = errno;
        if (saved_errno == ENOENT) {
            info("auxv=absent errno=%d (%s) fallback=informational",
                 saved_errno, strerror(saved_errno));
        } else {
            info("auxv=unreadable errno=%d (%s)", saved_errno,
                 strerror(saved_errno));
        }
        return;
    }
    size_t used = 0;
    int failed_errno = 0;
    while (used < sizeof(buffer)) {
        ssize_t count = read(fd, buffer + used, sizeof(buffer) - used);
        if (count < 0) {
            failed_errno = errno;
            break;
        }
        if (count == 0) {
            break;
        }
        used += (size_t)count;
    }
    if (close(fd) != 0 && failed_errno == 0) {
        failed_errno = errno;
    }
    if (failed_errno != 0) {
        info("auxv=read-error errno=%d (%s)", failed_errno,
             strerror(failed_errno));
        return;
    }

    struct auxv_expectation {
        int type;
        const char *name;
    };
    static const struct auxv_expectation expected[] = {
        {TK_AT_ENTRY, "AT_ENTRY"},   {TK_AT_PHDR, "AT_PHDR"},
        {TK_AT_PAGESZ, "AT_PAGESZ"}, {TK_AT_HWCAP, "AT_HWCAP"},
        {TK_AT_HWCAP2, "AT_HWCAP2"}, {TK_AT_RANDOM, "AT_RANDOM"},
        {TK_AT_UID, "AT_UID"},       {TK_AT_EXECFN, "AT_EXECFN"},
    };
    const size_t expected_count = sizeof(expected) / sizeof(expected[0]);
    unsigned char found[sizeof(expected) / sizeof(expected[0])];
    uint64_t values[sizeof(expected) / sizeof(expected[0])];
    memset(found, 0, sizeof(found));
    memset(values, 0, sizeof(values));

    const int truncated = (used % AUXV_PAIR_BYTES) != 0;
    const size_t entries = used / AUXV_PAIR_BYTES;
    int terminated = 0;
    size_t terminated_index = 0;
    for (size_t index = 0; index < entries; ++index) {
        const uint64_t type =
            load_u64_native(buffer + index * AUXV_PAIR_BYTES);
        const uint64_t value =
            load_u64_native(buffer + index * AUXV_PAIR_BYTES + 8U);
        if (type == (uint64_t)TK_AT_NULL) {
            terminated = 1;
            terminated_index = index;
            break;
        }
        for (size_t slot = 0; slot < expected_count; ++slot) {
            if ((uint64_t)expected[slot].type == type) {
                found[slot] = 1;
                values[slot] = value;
            }
        }
    }

    info("auxv=present bytes=%zu entries=%zu truncated=%d terminated=%d "
         "buffer_full=%d",
         used, entries, truncated, terminated,
         used == sizeof(buffer) ? 1 : 0);
    if (truncated != 0 || terminated == 0) {
        info("auxv-malformed bytes=%zu entries=%zu truncated=%d "
             "unterminated=%d reason=%s",
             used, entries, truncated, terminated == 0 ? 1 : 0,
             truncated != 0 ? "partial-pair" : "no-AT_NULL");
    }
    fputs("THEKERNEL_PROC_SHAPE_INFO auxv_types", stdout);
    for (size_t slot = 0; slot < expected_count; ++slot) {
        printf(" %s=%d", expected[slot].name, found[slot] ? 1 : 0);
    }
    fputc('\n', stdout);
    info("auxv_values AT_ENTRY=0x%" PRIx64 " AT_PHDR=0x%" PRIx64
         " AT_PAGESZ=0x%" PRIx64 " AT_HWCAP=0x%" PRIx64
         " AT_HWCAP2=0x%" PRIx64 " AT_NULL_index=%zu",
         values[0], values[1], values[2], values[3], values[4],
         terminated != 0 ? terminated_index : entries);
}

static size_t count_nul_entries(const char *data, size_t length) {
    size_t entries = 0;
    size_t index = 0;
    while (index < length) {
        const size_t start = index;
        while (index < length && data[index] != '\0') {
            index += 1U;
        }
        if (index > start) {
            entries += 1U;
        }
        if (index < length) {
            index += 1U;
        }
    }
    return entries;
}

/* INFORMATIONAL 5a: /proc/self/exe, /proc/self/cmdline, /proc/self/environ.
 *
 * Programs locate their own installation, re-exec themselves, and inspect the
 * environment through these files.  Each has a documented fallback
 * (argv/environ from the stack, /proc/self/maps for the executable), so an
 * absent file is reported and never fails the probe. */
static void report_self_file(const char *path, const char *label,
                             size_t capacity, enum self_file_shape shape) {
    struct file_bytes bytes;
    if (read_path_bounded(path, capacity, &bytes) != 0) {
        const int saved_errno = errno;
        info("%s=unreadable errno=%d (%s)", label, saved_errno,
             strerror(saved_errno));
        return;
    }
    if (shape == SELF_FILE_NUL_SEPARATED) {
        const size_t entries = count_nul_entries(read_buffer, bytes.length);
        info("%s=readable bytes=%zu entries=%zu truncated=%d", label,
             bytes.length, entries, bytes.truncated);
        return;
    }
    const int elf_magic = bytes.length >= 4U &&
                          (unsigned char)read_buffer[0] == 0x7fU &&
                          read_buffer[1] == 'E' && read_buffer[2] == 'L' &&
                          read_buffer[3] == 'F';
    info("%s=readable bytes=%zu elf_magic=%d", label, bytes.length, elf_magic);
}

static void report_self_link(const char *path, const char *label) {
    const ssize_t count = readlink(path, link_buffer, sizeof(link_buffer) - 1U);
    if (count < 0) {
        const int saved_errno = errno;
        info("%s=unreadable errno=%d (%s)", label, saved_errno,
             strerror(saved_errno));
        return;
    }
    link_buffer[count] = '\0';
    info("%s=readable bytes=%zd target=%s target_truncated=%d", label, count,
         link_buffer, count == (ssize_t)(sizeof(link_buffer) - 1U) ? 1 : 0);
}

static void report_proc_self_metadata(void) {
    report_self_file("/proc/self/exe", "exe", EXE_READ_CAPACITY,
                     SELF_FILE_BINARY);
    report_self_link("/proc/self/exe", "exe_target");
    report_self_file("/proc/self/cmdline", "cmdline", SELF_FILE_CAPACITY,
                     SELF_FILE_NUL_SEPARATED);
    report_self_file("/proc/self/environ", "environ", SELF_FILE_CAPACITY,
                     SELF_FILE_NUL_SEPARATED);
}

/* REQUIRED 6a: mmap() with an explicit hint is honoured or refuses cleanly.
 *
 * A compiler's allocator and QEMU's TCG region cache both pass hints to keep
 * related blocks near each other; taking the hint is an optimisation, but
 * returning a mapping the caller cannot use is a correctness bug.  So the
 * contract is exactly: the address is either the hint, or some other address
 * that is writable and reads back. */
static int check_mmap_hint(void) {
    const size_t bytes = HINT_PROBE_PAGES * page_bytes;
    void *scratch = mmap(NULL, bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (scratch == MAP_FAILED) {
        return failf("mmap-hint-scratch", "bytes=%zu", bytes);
    }
    void *hint = scratch;
    if (munmap(scratch, bytes) != 0) {
        return failf("munmap-hint-scratch", "address=%p bytes=%zu", scratch,
                     bytes);
    }

    errno = 0;
    void *mapping = mmap(hint, bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    const int saved_errno = errno;
    if (mapping == MAP_FAILED) {
        errno = saved_errno;
        return failf("mmap-hint", "hint=%p bytes=%zu", hint, bytes);
    }
    const int hint_taken = (mapping == hint);
    fill_anonymous_pattern(mapping, HINT_PROBE_PAGES);
    const int usable = anonymous_pattern_intact(mapping, HINT_PROBE_PAGES);
    info("mmap_hint hint=%p result=%p hint_taken=%d usable=%d bytes=%zu", hint,
         mapping, hint_taken, usable, bytes);

    int result = 0;
    if (usable == 0) {
        errno = EIO;
        result = failf("mmap-hint-unusable",
                       "hint=%p result=%p hint_taken=%d bytes=%zu "
                       "reason=readback-mismatch",
                       hint, mapping, hint_taken, bytes);
    }
    (void)munmap(mapping, bytes);
    return result;
}

/* REQUIRED 6b: MAP_FIXED_NOREPLACE either reserves the exact range or fails
 * with EEXIST.
 *
 * This is the flag a compiler uses to place a heap, a JIT arena, or a guest
 * physical-memory window at a fixed address without silently destroying an
 * existing mapping.  Two contracts are checked:
 *   - an occupied range must fail with EEXIST and leave the occupant's bytes
 *     untouched (a kernel that treats the flag as a hint returns a different
 *     address, and a kernel that treats it as MAP_FIXED destroys data);
 *   - an unoccupied range must be granted at exactly the requested address. */
static int check_mmap_fixed_noreplace(void) {
    int result = 0;
    const size_t busy_bytes = anon_region_total_bytes;
    int region_replaced = 0;

    errno = 0;
    void *replacement =
        mmap(anon_region_base, busy_bytes, PROT_READ | PROT_WRITE,
             MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
    const int occupied_errno = errno;
    if (replacement != MAP_FAILED) {
        region_replaced = 1;
        errno = occupied_errno;
        result = failf("mmap-fixed-noreplace-occupied",
                       "address=%p bytes=%zu returned=%p expected=MAP_FAILED "
                       "reason=occupant-replaced",
                       anon_region_base, busy_bytes, replacement);
        /* The occupant is gone; do not treat its contents as evidence again.
         * The replacement is a valid mapping, so releasing it is safe. */
        (void)munmap(anon_region_base, busy_bytes);
    } else if (occupied_errno != EEXIST) {
        errno = occupied_errno;
        result = failf("mmap-fixed-noreplace-occupied",
                       "address=%p bytes=%zu errno_observed=%d "
                       "errno_expected=%d(EEXIST) reason=refused-with-wrong-errno",
                       anon_region_base, busy_bytes, occupied_errno, EEXIST);
    }
    int survived = 0;
    if (region_replaced == 0) {
        survived =
            anonymous_pattern_intact(anon_region_base, ANON_PROBE_PAGES);
        if (survived == 0) {
            errno = EIO;
            result = failf("mmap-fixed-noreplace-clobbered",
                           "address=%p bytes=%zu reason=occupant-contents-lost",
                           anon_region_base, busy_bytes);
        }
    }
    info("mmap_fixed_noreplace_occupied address=%p bytes=%zu refused=%d "
         "errno_observed=%d errno_expected=%d occupant_survived=%d",
         anon_region_base, busy_bytes,
         (region_replaced == 0 && occupied_errno == EEXIST) ? 1 : 0,
         occupied_errno, EEXIST, survived);

    const size_t free_bytes = FIXED_PROBE_PAGES * page_bytes;
    void *scratch = mmap(NULL, free_bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (scratch == MAP_FAILED) {
        errno = 0;
        return failf("mmap-fixed-noreplace-scratch", "bytes=%zu", free_bytes);
    }
    void *requested = scratch;
    if (munmap(scratch, free_bytes) != 0) {
        return failf("munmap-fixed-noreplace-scratch",
                     "address=%p bytes=%zu", scratch, free_bytes);
    }

    errno = 0;
    void *granted = mmap(requested, free_bytes, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1,
                         0);
    const int granted_errno = errno;
    if (granted == MAP_FAILED) {
        errno = granted_errno;
        return failf("mmap-fixed-noreplace-free",
                     "requested=%p bytes=%zu reason=refused-unoccupied-range",
                     requested, free_bytes);
    }
    const int exact = (granted == requested);
    fill_anonymous_pattern(granted, FIXED_PROBE_PAGES);
    const int usable = anonymous_pattern_intact(granted, FIXED_PROBE_PAGES);
    info("mmap_fixed_noreplace_free requested=%p granted=%p exact=%d usable=%d "
         "bytes=%zu",
         requested, granted, exact, usable, free_bytes);
    if (exact == 0) {
        errno = granted_errno;
        result = failf("mmap-fixed-noreplace-free",
                       "requested=%p granted=%p bytes=%zu reason=address-not-"
                       "honoured",
                       requested, granted, free_bytes);
    }
    if (usable == 0) {
        errno = EIO;
        result = failf("mmap-fixed-noreplace-free",
                       "requested=%p granted=%p bytes=%zu reason=readback-"
                       "mismatch",
                       requested, granted, free_bytes);
    }
    (void)munmap(granted, free_bytes);
    return result;
}

/* INFORMATIONAL 7: address placement summary, plus the one required property.
 *
 * The numbers themselves are reconnaissance for the JIT: where a fresh
 * anonymous mapping lands, where the program's own code lives, and where the
 * libc heap starts.  Nothing is asserted about the values except that a fresh
 * mmap is page-aligned -- an unaligned mapping cannot be used at all. */
static int report_placement(void) {
    void *fresh = mmap(NULL, page_bytes, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (fresh == MAP_FAILED) {
        return failf("placement-fresh-mmap", "bytes=%zu", page_bytes);
    }
    void *heap = malloc(64U * 1024U);
    info("placement fresh_mmap=%p main=%p malloc=%p page_bytes=%zu",
         fresh, (void *)(uintptr_t)&main, heap, page_bytes);
    int result = 0;
    if (((uintptr_t)fresh % (uintptr_t)page_bytes) != 0) {
        errno = EFAULT;
        result = failf("placement-fresh-mmap-alignment",
                       "fresh_mmap=%p page_bytes=%zu reason=not-page-aligned",
                       fresh, page_bytes);
    }
    if (heap == NULL) {
        info("malloc=unavailable errno=%d (%s) fallback=informational", errno,
             strerror(errno));
    } else {
        const size_t heap_bytes = 64U * 1024U;
        volatile unsigned char *heap_bytes_view = heap;
        heap_bytes_view[0] = 0x5aU;
        heap_bytes_view[heap_bytes - 1U] = 0xa5U;
        const int usable = heap_bytes_view[0] == 0x5aU &&
                           heap_bytes_view[heap_bytes - 1U] == 0xa5U;
        info("malloc usable=%d bytes=%zu", usable, heap_bytes);
        free(heap);
    }
    (void)munmap(fresh, page_bytes);

    /* INFORMATIONAL: the libc queries a sizing consumer actually makes.  A
     * JIT sizing its code cache from the host's physical page count reads
     * _SC_PHYS_PAGES, and musl answers it from /proc/meminfo; a consumer that
     * cannot find that file falls back to a default rather than failing, so
     * these values are reported instead of asserted.  _SC_NPROCESSORS_ONLN
     * and the uname identity are recorded for the same reason. */
    long phys_pages = sysconf(_SC_PHYS_PAGES);
    info("sysconf phys_pages=%ld errno=%d (%s) available_pages=%ld "
         "processors_onln=%ld",
         phys_pages, phys_pages < 0 ? errno : 0,
         phys_pages < 0 ? strerror(errno) : "n/a",
         sysconf(_SC_AVPHYS_PAGES), sysconf(_SC_NPROCESSORS_ONLN));
    struct utsname identity;
    if (uname(&identity) == 0) {
        info("uname sysname=%s release=%s machine=%s", identity.sysname,
             identity.release, identity.machine);
    } else {
        info("uname=unavailable errno=%d (%s)", errno, strerror(errno));
    }
    long cpus_configured = sysconf(_SC_NPROCESSORS_CONF);
    info("sysconf processors_conf=%ld clock_ticks=%ld", cpus_configured,
         sysconf(_SC_CLK_TCK));
    return result;
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    setvbuf(stderr, NULL, _IOLBF, 0);

    const long page_size_value = sysconf(_SC_PAGESIZE);
    if (page_size_value <= 0) {
        errno = EINVAL;
        return failf("page-size", "sysconf_SC_PAGESIZE=%ld", page_size_value);
    }
    if (((unsigned long)page_size_value &
         ((unsigned long)page_size_value - 1UL)) != 0UL) {
        errno = EINVAL;
        return failf("page-size", "sysconf_SC_PAGESIZE=%ld "
                     "reason=not-a-power-of-two",
                     page_size_value);
    }
    page_bytes = (size_t)page_size_value;
    info("page_bytes=%zu", page_bytes);

    /* The probe's own scaffolding: everything below either reports on this
     * region or depends on anonymous mmap working at all. */
    if (create_anonymous_probe_region() != 0) {
        return EXIT_FAILURE;
    }

    int failed = 0;
    const struct maps_expectation expectation = {
        .code = (const void *)(uintptr_t)&main,
        .anon_base = anon_region_base,
        .anon_bytes = anon_region_total_bytes,
    };
    if (check_proc_self_maps(&expectation) != 0) {
        failed = 1;
    }
    if (check_proc_self_status() != 0) {
        failed = 1;
    }
    if (check_proc_cpuinfo() != 0) {
        failed = 1;
    }

    /* Informational sections: their findings are evidence, not verdicts. */
    report_proc_self_auxv();
    report_proc_self_metadata();

    if (check_mmap_hint() != 0) {
        failed = 1;
    }
    if (check_mmap_fixed_noreplace() != 0) {
        failed = 1;
    }
    if (report_placement() != 0) {
        failed = 1;
    }

    release_anonymous_probe_region();

    if (failed != 0) {
        const int saved_errno = errno;
        fprintf(stderr,
                "THEKERNEL_PROC_SHAPE_FAIL summary required_failures=%d "
                "errno=%d (%s)\n",
                required_failures, saved_errno, strerror(saved_errno));
        return EXIT_FAILURE;
    }
    puts("THEKERNEL_PROC_SHAPE_OK");
    return EXIT_SUCCESS;
}
