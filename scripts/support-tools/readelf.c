#define _GNU_SOURCE

#include <elf.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static void usage(FILE *stream)
{
    fprintf(stream, "Usage: readelf -h FILE\n");
}

static void die_errno(const char *path)
{
    fprintf(stderr, "readelf: %s: %s\n", path, strerror(errno));
    exit(1);
}

static void die_msg(const char *msg)
{
    fprintf(stderr, "readelf: %s\n", msg);
    exit(1);
}

static const char *class_name(unsigned char value)
{
    switch (value) {
    case ELFCLASS32:
        return "ELF32";
    case ELFCLASS64:
        return "ELF64";
    default:
        return "invalid";
    }
}

static const char *data_name(unsigned char value)
{
    switch (value) {
    case ELFDATA2LSB:
        return "2's complement, little endian";
    case ELFDATA2MSB:
        return "2's complement, big endian";
    default:
        return "invalid data encoding";
    }
}

int main(int argc, char **argv)
{
    const char *path = NULL;

    if (argc == 2 && strcmp(argv[1], "--help") == 0) {
        usage(stdout);
        return 0;
    }

    if (argc == 3 && strcmp(argv[1], "-h") == 0) {
        path = argv[2];
    } else {
        usage(stderr);
        return 1;
    }

    FILE *file = fopen(path, "rb");
    if (!file) {
        die_errno(path);
    }

    unsigned char ident[EI_NIDENT];
    if (fread(ident, 1, sizeof(ident), file) != sizeof(ident)) {
        fclose(file);
        die_msg("failed to read ELF header");
    }
    fclose(file);

    if (memcmp(ident, ELFMAG, SELFMAG) != 0) {
        die_msg("not an ELF file");
    }

    printf("ELF Header:\n");
    printf("  Class:                             %s\n", class_name(ident[EI_CLASS]));
    printf("  Data:                              %s\n", data_name(ident[EI_DATA]));
    return 0;
}
