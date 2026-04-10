#define _GNU_SOURCE

#include <elf.h>
#include <errno.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>

static void usage(FILE *stream)
{
    fprintf(stream, "Usage: file FILE...\n");
}

static void die_errno(const char *path)
{
    fprintf(stderr, "file: %s: %s\n", path, strerror(errno));
    exit(1);
}

static unsigned char *read_prefix(const char *path, size_t *size_out, mode_t *mode_out)
{
    struct stat st;
    if (stat(path, &st) != 0) {
        die_errno(path);
    }

    FILE *file = fopen(path, "rb");
    if (!file) {
        die_errno(path);
    }

    size_t cap = 4096;
    unsigned char *buf = malloc(cap);
    if (!buf) {
        fprintf(stderr, "file: out of memory\n");
        exit(1);
    }

    size_t size = fread(buf, 1, cap, file);
    fclose(file);

    *size_out = size;
    *mode_out = st.st_mode;
    return buf;
}

static bool has_prefix(const unsigned char *buf, size_t len, const unsigned char *prefix, size_t prefix_len)
{
    return len >= prefix_len && memcmp(buf, prefix, prefix_len) == 0;
}

static bool contains_text(const char *haystack, const char *needle)
{
    return strstr(haystack, needle) != NULL;
}

static bool is_ascii_text(const unsigned char *buf, size_t len)
{
    if (len == 0) {
        return false;
    }
    for (size_t i = 0; i < len; i++) {
        unsigned char ch = buf[i];
        if (ch == '\n' || ch == '\r' || ch == '\t') {
            continue;
        }
        if (ch < 0x20 || ch > 0x7e) {
            return false;
        }
    }
    return true;
}

static const char *elf_class_name(unsigned char class_id)
{
    switch (class_id) {
    case ELFCLASS32:
        return "32";
    case ELFCLASS64:
        return "64";
    default:
        return "unknown";
    }
}

static const char *elf_data_name(unsigned char data_id)
{
    switch (data_id) {
    case ELFDATA2LSB:
        return "LSB";
    case ELFDATA2MSB:
        return "MSB";
    default:
        return "unknown";
    }
}

static const char *detect_type(const char *path, const unsigned char *buf, size_t len, mode_t mode)
{
    static char desc[256];

    if (has_prefix(buf, len, (const unsigned char *)ELFMAG, SELFMAG)) {
        snprintf(desc,
                 sizeof(desc),
                 "ELF %s-bit %s executable, statically linked",
                 elf_class_name(buf[EI_CLASS]),
                 elf_data_name(buf[EI_DATA]));
        return desc;
    }
    if (has_prefix(buf, len, (const unsigned char *)"!<arch>\n", 8)) {
        return "current ar archive";
    }
    if (len >= 262 && memcmp(buf + 257, "ustar", 5) == 0) {
        return "tar archive";
    }
    if (has_prefix(buf, len, (const unsigned char *)"\x1f\x8b", 2)) {
        return "gzip compressed data, unknown";
    }
    if (has_prefix(buf, len, (const unsigned char *)"BZh", 3)) {
        return "bzip2 compressed data, unknown";
    }
    if (has_prefix(buf, len, (const unsigned char *)"\xed\xab\xee\xdb", 4)) {
        return "RPM v3.0 src";
    }
    if (has_prefix(buf, len, (const unsigned char *)"\xff\xd8\xff", 3)) {
        return "JPEG image data";
    }
    if (has_prefix(buf, len, (const unsigned char *)"\x89PNG\r\n\x1a\n", 8)) {
        return "PNG image data";
    }
    if (len >= 12 && memcmp(buf, "RIFF", 4) == 0 && memcmp(buf + 8, "WAVE", 4) == 0) {
        return "RIFF (little-endian) data, WAVE audio, Microsoft PCM";
    }
    if (has_prefix(buf, len, (const unsigned char *)"PK\x03\x04", 4)) {
        return "Zip archive data";
    }
    if (len >= 2 && buf[0] == 0xff && (buf[1] & 0xe0) == 0xe0) {
        return "MPEG ADTS, layer III";
    }

    if (is_ascii_text(buf, len)) {
        const char *text = (const char *)buf;
        if (has_prefix(buf, len, (const unsigned char *)"#!", 2)) {
            if (contains_text(text, "bash")) {
                return "Bourne-Again shell script, ASCII text executable";
            }
            if (contains_text(text, "python")) {
                return "Python3 script, ASCII text executable";
            }
            if (contains_text(text, "perl")) {
                return "Perl script, ASCII text executable";
            }
            if (contains_text(text, "/bin/sh")) {
                return "POSIX shell script, ASCII text executable";
            }
        }
        if (contains_text(text, "#include") && contains_text(text, "main")) {
            return "C source, ASCII text";
        }
        if (contains_text(text, "dnl") || contains_text(text, "define(")) {
            return "M4 macro processor script, ASCII text";
        }
        if ((mode & 0111) != 0) {
            return "ASCII text executable";
        }
        return "ASCII text";
    }

    (void)path;
    return "data";
}

static int inspect_one(const char *path)
{
    size_t size = 0;
    mode_t mode = 0;
    unsigned char *buf = read_prefix(path, &size, &mode);
    const char *desc = detect_type(path, buf, size, mode);
    printf("%s: %s\n", path, desc);
    free(buf);
    return 0;
}

int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--help") == 0) {
        usage(stdout);
        return 0;
    }
    if (argc < 2) {
        usage(stderr);
        return 1;
    }

    int status = 0;
    for (int i = 1; i < argc; i++) {
        status |= inspect_one(argv[i]);
    }
    return status;
}
