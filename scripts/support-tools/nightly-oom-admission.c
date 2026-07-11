#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define OVERCOMMIT_POLICY_PATH "/proc/sys/vm/overcommit_memory"
#define OVERCOMMIT_RATIO_PATH "/proc/sys/vm/overcommit_ratio"

static int parse_size(const char *text, size_t *value)
{
    char *end = NULL;
    unsigned long long parsed;

    errno = 0;
    parsed = strtoull(text, &end, 10);
    if (errno != 0 || !text || *text == '\0' || !end || *end != '\0' ||
        parsed == 0 || parsed > SIZE_MAX) {
        return -1;
    }
    *value = (size_t)parsed;
    return 0;
}

static int read_uint_file(const char *path, unsigned int *value)
{
    char buffer[32];
    char *end = NULL;
    unsigned long parsed;
    ssize_t length;
    int fd;

    fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    length = read(fd, buffer, sizeof(buffer) - 1);
    if (close(fd) != 0 || length <= 0) {
        return -1;
    }
    buffer[length] = '\0';
    errno = 0;
    parsed = strtoul(buffer, &end, 10);
    if (errno != 0 || end == buffer || parsed > UINT_MAX) {
        return -1;
    }
    while (*end == ' ' || *end == '\t' || *end == '\r' || *end == '\n') {
        ++end;
    }
    if (*end != '\0') {
        return -1;
    }
    *value = (unsigned int)parsed;
    return 0;
}

static int write_uint_file(const char *path, unsigned int value)
{
    char buffer[32];
    size_t offset = 0;
    int length;
    int fd;

    length = snprintf(buffer, sizeof(buffer), "%u\n", value);
    if (length <= 0 || (size_t)length >= sizeof(buffer)) {
        return -1;
    }
    fd = open(path, O_WRONLY | O_TRUNC);
    if (fd < 0) {
        return -1;
    }
    while (offset < (size_t)length) {
        ssize_t written = write(fd, buffer + offset, (size_t)length - offset);
        if (written <= 0) {
            close(fd);
            return -1;
        }
        offset += (size_t)written;
    }
    return close(fd);
}

static int expect_mapping(size_t length, int expect_success)
{
    unsigned char *mapping;
    int mapping_errno;

    errno = 0;
    mapping = mmap(NULL, length, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    mapping_errno = errno;
    if (!expect_success) {
        if (mapping != MAP_FAILED) {
            munmap(mapping, length);
            fprintf(stderr,
                    "OOM admission unexpectedly mapped %zu bytes\n", length);
            return 1;
        }
        if (mapping_errno != ENOMEM) {
            fprintf(stderr,
                    "OOM admission returned errno %d instead of ENOMEM\n",
                    mapping_errno);
            return 1;
        }
        puts("NIGHTLY_OOM_EXPECTED_ENOMEM");
        return 0;
    }

    if (mapping == MAP_FAILED) {
        fprintf(stderr, "recovery mmap failed with errno %d\n", mapping_errno);
        return 1;
    }
    mapping[0] = 0x5a;
    mapping[length - 1] = 0xa5;
    if (mapping[0] != 0x5a || mapping[length - 1] != 0xa5 ||
        munmap(mapping, length) != 0) {
        fputs("recovery mapping verification failed\n", stderr);
        return 1;
    }
    puts("NIGHTLY_OOM_RECOVERY_MAPPING_OK");
    return 0;
}

static int strict_overcommit_failure(size_t length)
{
    unsigned int old_policy;
    unsigned int old_ratio;
    void *mapping;
    int mapping_errno;
    int restore_policy_status;
    int restore_ratio_status;

    if (read_uint_file(OVERCOMMIT_POLICY_PATH, &old_policy) != 0 ||
        read_uint_file(OVERCOMMIT_RATIO_PATH, &old_ratio) != 0) {
        fputs("failed to read overcommit policy\n", stderr);
        return 1;
    }
    if (write_uint_file(OVERCOMMIT_RATIO_PATH, 1) != 0) {
        fputs("failed to set strict overcommit ratio\n", stderr);
        return 1;
    }
    if (write_uint_file(OVERCOMMIT_POLICY_PATH, 2) != 0) {
        write_uint_file(OVERCOMMIT_RATIO_PATH, old_ratio);
        fputs("failed to enable strict overcommit policy\n", stderr);
        return 1;
    }

    errno = 0;
    mapping = mmap(NULL, length, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    mapping_errno = errno;
    if (mapping != MAP_FAILED) {
        munmap(mapping, length);
    }

    /* Lift the strict policy before stdio or recovery allocation can run. */
    restore_policy_status = write_uint_file(OVERCOMMIT_POLICY_PATH, old_policy);
    restore_ratio_status = write_uint_file(OVERCOMMIT_RATIO_PATH, old_ratio);
    if (restore_policy_status != 0 || restore_ratio_status != 0) {
        fputs("failed to restore overcommit policy\n", stderr);
        return 1;
    }
    if (mapping != MAP_FAILED) {
        fprintf(stderr,
                "OOM admission unexpectedly mapped %zu bytes\n", length);
        return 1;
    }
    if (mapping_errno != ENOMEM) {
        fprintf(stderr,
                "OOM admission returned errno %d instead of ENOMEM\n",
                mapping_errno);
        return 1;
    }
    puts("NIGHTLY_OOM_EXPECTED_ENOMEM");
    return expect_mapping(4096, 1);
}

int main(int argc, char **argv)
{
    size_t length;

    if (argc != 3 ||
        (strcmp(argv[1], "--expect-failure") != 0 &&
         strcmp(argv[1], "--expect-success") != 0 &&
         strcmp(argv[1], "--strict-overcommit-failure") != 0) ||
        parse_size(argv[2], &length) != 0) {
        fprintf(stderr,
                "usage: %s {--expect-failure|--expect-success|"
                "--strict-overcommit-failure} BYTES\n",
                argv[0]);
        return 2;
    }
    if (strcmp(argv[1], "--strict-overcommit-failure") == 0) {
        return strict_overcommit_failure(length);
    }
    return expect_mapping(length, strcmp(argv[1], "--expect-success") == 0);
}
