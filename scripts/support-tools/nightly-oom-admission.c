#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

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

int main(int argc, char **argv)
{
    int expect_success;
    size_t length;
    unsigned char *mapping;

    if (argc != 3 ||
        (strcmp(argv[1], "--expect-failure") != 0 &&
         strcmp(argv[1], "--expect-success") != 0) ||
        parse_size(argv[2], &length) != 0) {
        fprintf(stderr,
                "usage: %s {--expect-failure|--expect-success} BYTES\n",
                argv[0]);
        return 2;
    }
    expect_success = strcmp(argv[1], "--expect-success") == 0;

    errno = 0;
    mapping = mmap(NULL, length, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!expect_success) {
        if (mapping != MAP_FAILED) {
            munmap(mapping, length);
            fprintf(stderr,
                    "OOM admission unexpectedly mapped %zu bytes\n", length);
            return 1;
        }
        if (errno != ENOMEM) {
            fprintf(stderr,
                    "OOM admission returned errno %d instead of ENOMEM\n", errno);
            return 1;
        }
        puts("NIGHTLY_OOM_EXPECTED_ENOMEM");
        return 0;
    }

    if (mapping == MAP_FAILED) {
        fprintf(stderr, "recovery mmap failed with errno %d\n", errno);
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
