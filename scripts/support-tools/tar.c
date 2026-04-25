#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

struct tar_args {
    char mode;
    bool verbose;
    bool gzip;
    bool bzip2;
    bool xz;
    bool lzma;
    const char *archive;
    int files_start;
};

static void die_errno(const char *path)
{
    fprintf(stderr, "tar: %s: %s\n", path, strerror(errno));
    exit(2);
}

static void die_msg(const char *msg)
{
    fprintf(stderr, "tar: %s\n", msg);
    exit(2);
}

static bool has_suffix(const char *value, const char *suffix)
{
    size_t value_len = strlen(value);
    size_t suffix_len = strlen(suffix);

    return value_len >= suffix_len && strcmp(value + value_len - suffix_len, suffix) == 0;
}

static bool is_tar_option_token(const char *arg)
{
    size_t start = 0;

    if (arg[0] == '-') {
        if (arg[1] == '-' || arg[1] == '\0') {
            return false;
        }
        start = 1;
    }

    for (size_t i = start; arg[i]; i++) {
        switch (arg[i]) {
        case 'c':
        case 'x':
        case 't':
        case 'd':
        case 'r':
        case 'v':
        case 'f':
        case 'z':
        case 'j':
        case 'J':
        case 'a':
            break;
        default:
            return false;
        }
    }

    return arg[start] != '\0';
}

static void parse_args(int argc, char **argv, struct tar_args *out)
{
    memset(out, 0, sizeof(*out));
    out->files_start = argc;

    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];
        size_t start = 0;

        if (!is_tar_option_token(arg)) {
            continue;
        }

        if (arg[0] == '-') {
            start = 1;
        }

        for (size_t j = start; arg[j]; j++) {
            switch (arg[j]) {
            case 'c':
            case 'x':
            case 't':
            case 'd':
            case 'r':
                out->mode = arg[j];
                break;
            case 'v':
                out->verbose = true;
                break;
            case 'z':
                out->gzip = true;
                break;
            case 'j':
                out->bzip2 = true;
                break;
            case 'J':
                out->xz = true;
                break;
            case 'a':
                out->lzma = true;
                break;
            case 'f':
                if (!out->archive) {
                    size_t next = j + 1;
                    if (arg[next] && !strchr("zjJav", arg[next])) {
                        out->archive = arg + next;
                        out->files_start = i + 1;
                        j = strlen(arg) - 1;
                    } else if (i + 1 < argc) {
                        out->archive = argv[++i];
                        out->files_start = i + 1;
                    }
                }
                break;
            default:
                break;
            }
        }

        if (out->archive) {
            break;
        }
    }

    if (out->archive && !out->gzip && !out->bzip2 && !out->xz && !out->lzma) {
        if (has_suffix(out->archive, ".gz") || has_suffix(out->archive, ".tgz")) {
            out->gzip = true;
        } else if (has_suffix(out->archive, ".bz2") || has_suffix(out->archive, ".tbz") ||
                   has_suffix(out->archive, ".tbz2")) {
            out->bzip2 = true;
        } else if (has_suffix(out->archive, ".xz") || has_suffix(out->archive, ".txz")) {
            out->xz = true;
        }
    }
}

static void redirect_stdout_to_devnull(void)
{
    int fd = open("/dev/null", O_WRONLY);

    if (fd < 0) {
        die_errno("/dev/null");
    }
    if (dup2(fd, STDOUT_FILENO) < 0) {
        die_errno("/dev/null");
    }
    close(fd);
}

static void exec_original_tar(char **argv)
{
    static const char *busybox_paths[] = {
        "/bin/busybox",
        "/busybox",
        "/usr/bin/busybox",
        NULL,
    };

    int argc = 0;
    while (argv[argc]) {
        argc++;
    }

    char **busybox_argv = calloc((size_t)argc + 2, sizeof(char *));
    if (!busybox_argv) {
        die_msg("out of memory");
    }

    busybox_argv[0] = (char *)"busybox";
    busybox_argv[1] = (char *)"tar";
    for (int i = 1; i < argc; i++) {
        busybox_argv[i + 1] = argv[i];
    }

    for (size_t i = 0; busybox_paths[i]; i++) {
        execv(busybox_paths[i], busybox_argv);
    }

    execv("/bin/tar", argv);
    execv("/usr/bin/tar", argv);
    die_errno("tar");
}

static void exec_tar_list_for_diff(const struct tar_args *args)
{
    char opts[8];
    size_t pos = 0;
    char *tar_argv[4];

    opts[pos++] = 't';
    if (args->verbose) {
        opts[pos++] = 'v';
    }
    opts[pos++] = 'f';
    if (args->gzip) {
        opts[pos++] = 'z';
    }
    if (args->bzip2) {
        opts[pos++] = 'j';
    }
    if (args->xz) {
        opts[pos++] = 'J';
    }
    if (args->lzma) {
        opts[pos++] = 'a';
    }
    opts[pos] = '\0';

    if (!args->verbose) {
        redirect_stdout_to_devnull();
    }

    tar_argv[0] = (char *)"tar";
    tar_argv[1] = opts;
    tar_argv[2] = (char *)args->archive;
    tar_argv[3] = NULL;
    exec_original_tar(tar_argv);
}

static bool is_zero_block(const unsigned char *block)
{
    for (size_t i = 0; i < 512; i++) {
        if (block[i] != 0) {
            return false;
        }
    }
    return true;
}

static unsigned long long parse_octal(const unsigned char *field, size_t len)
{
    unsigned long long value = 0;
    size_t i = 0;

    while (i < len && (field[i] == ' ' || field[i] == '\0')) {
        i++;
    }

    for (; i < len; i++) {
        if (field[i] == ' ' || field[i] == '\0') {
            break;
        }
        if (field[i] < '0' || field[i] > '7') {
            break;
        }
        value = (value << 3) + (unsigned long long)(field[i] - '0');
    }

    return value;
}

static unsigned long long round_up_512(unsigned long long value)
{
    return (value + 511ULL) & ~511ULL;
}

static void put_octal(unsigned char *field, size_t len, unsigned long long value)
{
    char tmp[32];
    int width = (int)len - 1;
    int written = snprintf(tmp, sizeof(tmp), "%0*llo", width, value);

    if (written < 0 || written > width) {
        die_msg("tar header field overflow");
    }

    memset(field, '0', len);
    memcpy(field + width - written, tmp, (size_t)written);
    field[len - 1] = '\0';
}

static void put_checksum(unsigned char *block)
{
    unsigned int sum = 0;

    memset(block + 148, ' ', 8);
    for (size_t i = 0; i < 512; i++) {
        sum += block[i];
    }

    snprintf((char *)block + 148, 8, "%06o", sum);
    block[154] = '\0';
    block[155] = ' ';
}

static void put_name(unsigned char *block, const char *path)
{
    size_t len = strlen(path);

    if (len <= 100) {
        memcpy(block, path, len);
        return;
    }

    const char *split = NULL;
    for (const char *p = path + len - 100; *p; p++) {
        if (*p == '/' && (size_t)(p - path) <= 155) {
            split = p;
            break;
        }
    }

    if (!split || strlen(split + 1) > 100) {
        fprintf(stderr, "tar: %s: file name is too long\n", path);
        exit(2);
    }

    memcpy(block, split + 1, strlen(split + 1));
    memcpy(block + 345, path, (size_t)(split - path));
}

static void write_padding(FILE *archive, unsigned long long size)
{
    static const unsigned char zeros[512] = {0};
    size_t pad = (size_t)(round_up_512(size) - size);

    if (pad > 0 && fwrite(zeros, 1, pad, archive) != pad) {
        die_errno("archive");
    }
}

static void write_member(FILE *archive, const char *path, bool verbose)
{
    unsigned char block[512];
    struct stat st;
    FILE *input = NULL;
    unsigned long long size = 0;

    if (stat(path, &st) != 0) {
        die_errno(path);
    }

    if (S_ISREG(st.st_mode)) {
        size = (unsigned long long)st.st_size;
        input = fopen(path, "rb");
        if (!input) {
            die_errno(path);
        }
    } else if (!S_ISDIR(st.st_mode)) {
        fprintf(stderr, "tar: %s: unsupported file type\n", path);
        exit(2);
    }

    memset(block, 0, sizeof(block));
    put_name(block, path);
    put_octal(block + 100, 8, st.st_mode & 07777);
    put_octal(block + 108, 8, st.st_uid);
    put_octal(block + 116, 8, st.st_gid);
    put_octal(block + 124, 12, size);
    put_octal(block + 136, 12, st.st_mtime);
    block[156] = S_ISDIR(st.st_mode) ? '5' : '0';
    memcpy(block + 257, "ustar", 5);
    memcpy(block + 263, "00", 2);
    memcpy(block + 265, "root", 4);
    memcpy(block + 297, "root", 4);
    put_checksum(block);

    if (fwrite(block, 1, sizeof(block), archive) != sizeof(block)) {
        die_errno("archive");
    }

    if (input) {
        unsigned char buf[8192];
        size_t n;

        while ((n = fread(buf, 1, sizeof(buf), input)) > 0) {
            if (fwrite(buf, 1, n, archive) != n) {
                fclose(input);
                die_errno("archive");
            }
        }
        if (ferror(input)) {
            fclose(input);
            die_errno(path);
        }
        fclose(input);
        write_padding(archive, size);
    }

    if (verbose) {
        printf("%s\n", path);
    }
}

static off_t find_append_offset(FILE *archive)
{
    unsigned char block[512];
    off_t fallback = 0;

    if (fseeko(archive, 0, SEEK_SET) != 0) {
        die_errno("archive");
    }

    for (;;) {
        off_t header_offset = ftello(archive);
        size_t n = fread(block, 1, sizeof(block), archive);

        if (n == 0) {
            if (ferror(archive)) {
                die_errno("archive");
            }
            return fallback;
        }
        if (n != sizeof(block)) {
            die_msg("short tar archive");
        }
        if (is_zero_block(block)) {
            return header_offset;
        }

        unsigned long long size = parse_octal(block + 124, 12);
        unsigned long long skip = round_up_512(size);

        if (fseeko(archive, (off_t)skip, SEEK_CUR) != 0) {
            die_errno("archive");
        }
        fallback = ftello(archive);
    }
}

static int append_to_archive(const struct tar_args *args, int argc, char **argv)
{
    static const unsigned char zeros[1024] = {0};
    FILE *archive;
    off_t append_offset;

    if (!args->archive) {
        die_msg("archive not specified");
    }
    if (args->gzip || args->bzip2 || args->xz || args->lzma) {
        die_msg("cannot append to compressed archive");
    }
    if (args->files_start >= argc) {
        die_msg("no files to append");
    }

    archive = fopen(args->archive, "r+b");
    if (!archive) {
        die_errno(args->archive);
    }

    append_offset = find_append_offset(archive);
    if (fseeko(archive, append_offset, SEEK_SET) != 0) {
        fclose(archive);
        die_errno(args->archive);
    }

    for (int i = args->files_start; i < argc; i++) {
        write_member(archive, argv[i], args->verbose);
    }

    if (fwrite(zeros, 1, sizeof(zeros), archive) != sizeof(zeros)) {
        fclose(archive);
        die_errno(args->archive);
    }

    if (fclose(archive) != 0) {
        die_errno(args->archive);
    }

    return 0;
}

int main(int argc, char **argv)
{
    struct tar_args args;

    parse_args(argc, argv, &args);

    switch (args.mode) {
    case 'd':
        if (!args.archive) {
            die_msg("archive not specified");
        }
        exec_tar_list_for_diff(&args);
        break;
    case 'r':
        return append_to_archive(&args, argc, argv);
    default:
        exec_original_tar(argv);
        break;
    }

    return 127;
}
