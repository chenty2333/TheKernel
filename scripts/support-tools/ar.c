#define _GNU_SOURCE
#define _FILE_OFFSET_BITS 64

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>
#include <utime.h>

#define ARMAG "!<arch>\n"
#define SARMAG 8
#define ARFMAG "`\n"

struct ar_hdr {
    char ar_name[16];
    char ar_date[12];
    char ar_uid[6];
    char ar_gid[6];
    char ar_mode[8];
    char ar_size[10];
    char ar_fmag[2];
};

typedef struct {
    char *name;
    mode_t mode;
    uid_t uid;
    gid_t gid;
    time_t mtime;
    unsigned char *data;
    size_t size;
} Member;

typedef struct {
    Member *items;
    size_t len;
    size_t cap;
} Archive;

typedef struct {
    char op;
    char pos_kind;
    bool verbose;
    bool update_only;
    bool create;
    const char *pos_name;
    const char *archive_path;
    char **members;
    size_t member_count;
} Options;

static void fatal_errno(const char *what)
{
    fprintf(stderr, "ar: %s: %s\n", what, strerror(errno));
    exit(1);
}

static void fatal_msg(const char *msg)
{
    fprintf(stderr, "ar: %s\n", msg);
    exit(1);
}

static void *xmalloc(size_t size)
{
    void *ptr = malloc(size ? size : 1);
    if (!ptr) {
        fatal_errno("malloc");
    }
    return ptr;
}

static void *xrealloc(void *ptr, size_t size)
{
    void *next = realloc(ptr, size ? size : 1);
    if (!next) {
        fatal_errno("realloc");
    }
    return next;
}

static char *xstrdup(const char *src)
{
    size_t len = strlen(src) + 1;
    char *dst = xmalloc(len);
    memcpy(dst, src, len);
    return dst;
}

static char *xstrndup(const char *src, size_t len)
{
    char *dst = xmalloc(len + 1);
    memcpy(dst, src, len);
    dst[len] = '\0';
    return dst;
}

static void member_destroy(Member *member)
{
    free(member->name);
    free(member->data);
    memset(member, 0, sizeof(*member));
}

static void archive_destroy(Archive *archive)
{
    for (size_t i = 0; i < archive->len; i++) {
        member_destroy(&archive->items[i]);
    }
    free(archive->items);
    memset(archive, 0, sizeof(*archive));
}

static void archive_reserve(Archive *archive, size_t need)
{
    if (archive->cap >= need) {
        return;
    }

    size_t cap = archive->cap ? archive->cap : 8;
    while (cap < need) {
        cap *= 2;
    }
    archive->items = xrealloc(archive->items, cap * sizeof(*archive->items));
    archive->cap = cap;
}

static void archive_push(Archive *archive, Member member)
{
    archive_reserve(archive, archive->len + 1);
    archive->items[archive->len++] = member;
}

static void archive_insert(Archive *archive, size_t index, Member member)
{
    if (index > archive->len) {
        index = archive->len;
    }
    archive_reserve(archive, archive->len + 1);
    memmove(&archive->items[index + 1],
            &archive->items[index],
            (archive->len - index) * sizeof(*archive->items));
    archive->items[index] = member;
    archive->len++;
}

static Member archive_take(Archive *archive, size_t index)
{
    Member member = archive->items[index];
    if (index + 1 < archive->len) {
        memmove(&archive->items[index],
                &archive->items[index + 1],
                (archive->len - index - 1) * sizeof(*archive->items));
    }
    archive->len--;
    return member;
}

static void archive_remove(Archive *archive, size_t index)
{
    Member member = archive_take(archive, index);
    member_destroy(&member);
}

static ssize_t archive_find(const Archive *archive, const char *name)
{
    for (size_t i = 0; i < archive->len; i++) {
        if (strcmp(archive->items[i].name, name) == 0) {
            return (ssize_t)i;
        }
    }
    return -1;
}

static const char *base_name(const char *path)
{
    const char *slash = strrchr(path, '/');
    return slash ? slash + 1 : path;
}

static long long parse_decimal_field(const char *src, size_t len)
{
    char buf[32];
    if (len >= sizeof(buf)) {
        fatal_msg("corrupt archive header");
    }
    memcpy(buf, src, len);
    buf[len] = '\0';

    size_t end = len;
    while (end > 0 && (buf[end - 1] == ' ' || buf[end - 1] == '\0')) {
        end--;
    }
    buf[end] = '\0';
    if (end == 0) {
        return 0;
    }

    return strtoll(buf, NULL, 10);
}

static unsigned long parse_octal_field(const char *src, size_t len)
{
    char buf[32];
    if (len >= sizeof(buf)) {
        fatal_msg("corrupt archive header");
    }
    memcpy(buf, src, len);
    buf[len] = '\0';

    size_t end = len;
    while (end > 0 && (buf[end - 1] == ' ' || buf[end - 1] == '\0')) {
        end--;
    }
    buf[end] = '\0';
    if (end == 0) {
        return 0;
    }

    return strtoul(buf, NULL, 8);
}

static void format_decimal_field(char *dst, size_t width, long long value)
{
    char buf[64];
    int written = snprintf(buf, sizeof(buf), "%lld", value);
    if (written < 0 || (size_t)written >= width + 1) {
        fatal_msg("archive field overflow");
    }
    memset(dst, ' ', width);
    memcpy(dst, buf, (size_t)written);
}

static void format_octal_field(char *dst, size_t width, unsigned long value)
{
    char buf[64];
    int written = snprintf(buf, sizeof(buf), "%lo", value);
    if (written < 0 || (size_t)written >= width + 1) {
        fatal_msg("archive field overflow");
    }
    memset(dst, ' ', width);
    memcpy(dst, buf, (size_t)written);
}

static char *parse_member_name(const struct ar_hdr *hdr,
                               const unsigned char *payload,
                               size_t payload_size,
                               size_t *name_prefix_size)
{
    char raw[17];
    memcpy(raw, hdr->ar_name, 16);
    raw[16] = '\0';

    size_t end = 16;
    while (end > 0 && raw[end - 1] == ' ') {
        end--;
    }
    raw[end] = '\0';

    if (strncmp(raw, "#1/", 3) == 0) {
        size_t name_len = (size_t)strtoull(raw + 3, NULL, 10);
        if (name_len > payload_size) {
            fatal_msg("corrupt archive member name");
        }
        *name_prefix_size = name_len;
        return xstrndup((const char *)payload, name_len);
    }

    *name_prefix_size = 0;
    end = strlen(raw);
    if (end > 0 && raw[end - 1] == '/') {
        raw[end - 1] = '\0';
    }
    return xstrdup(raw);
}

static void load_archive(const char *path, Archive *archive, bool allow_missing)
{
    memset(archive, 0, sizeof(*archive));

    FILE *fp = fopen(path, "rb");
    if (!fp) {
        if (allow_missing && errno == ENOENT) {
            return;
        }
        fatal_errno(path);
    }

    char magic[SARMAG];
    if (fread(magic, 1, sizeof(magic), fp) != sizeof(magic)) {
        fclose(fp);
        fatal_msg("invalid archive");
    }
    if (memcmp(magic, ARMAG, SARMAG) != 0) {
        fclose(fp);
        fatal_msg("invalid archive magic");
    }

    for (;;) {
        struct ar_hdr hdr;
        size_t got = fread(&hdr, 1, sizeof(hdr), fp);
        if (got == 0) {
            break;
        }
        if (got != sizeof(hdr)) {
            fclose(fp);
            fatal_msg("truncated archive header");
        }
        if (memcmp(hdr.ar_fmag, ARFMAG, 2) != 0) {
            fclose(fp);
            fatal_msg("invalid archive member header");
        }

        long long raw_size = parse_decimal_field(hdr.ar_size, sizeof(hdr.ar_size));
        if (raw_size < 0) {
            fclose(fp);
            fatal_msg("invalid archive member size");
        }
        size_t payload_size = (size_t)raw_size;
        unsigned char *payload = xmalloc(payload_size);
        if (payload_size > 0 && fread(payload, 1, payload_size, fp) != payload_size) {
            free(payload);
            fclose(fp);
            fatal_msg("truncated archive member");
        }
        if ((payload_size & 1U) != 0) {
            if (fgetc(fp) == EOF) {
                free(payload);
                fclose(fp);
                fatal_msg("truncated archive padding");
            }
        }

        size_t name_prefix_size = 0;
        char *name = parse_member_name(&hdr, payload, payload_size, &name_prefix_size);
        if (name_prefix_size > payload_size) {
            free(name);
            free(payload);
            fclose(fp);
            fatal_msg("corrupt archive member size");
        }

        size_t member_size = payload_size - name_prefix_size;
        unsigned char *data = xmalloc(member_size);
        if (member_size > 0) {
            memcpy(data, payload + name_prefix_size, member_size);
        }
        free(payload);

        Member member = {
            .name = name,
            .mode = (mode_t)parse_octal_field(hdr.ar_mode, sizeof(hdr.ar_mode)),
            .uid = (uid_t)parse_decimal_field(hdr.ar_uid, sizeof(hdr.ar_uid)),
            .gid = (gid_t)parse_decimal_field(hdr.ar_gid, sizeof(hdr.ar_gid)),
            .mtime = (time_t)parse_decimal_field(hdr.ar_date, sizeof(hdr.ar_date)),
            .data = data,
            .size = member_size,
        };
        archive_push(archive, member);
    }

    fclose(fp);
}

static void fill_member_header(struct ar_hdr *hdr, const Member *member)
{
    memset(hdr, ' ', sizeof(*hdr));

    size_t name_len = strlen(member->name);
    size_t header_size = member->size;

    if (name_len <= 15) {
        memcpy(hdr->ar_name, member->name, name_len);
        hdr->ar_name[name_len] = '/';
    } else {
        int written = snprintf(hdr->ar_name, sizeof(hdr->ar_name), "#1/%zu", name_len);
        if (written < 0 || (size_t)written >= sizeof(hdr->ar_name)) {
            fatal_msg("archive member name too long");
        }
        header_size += name_len;
    }

    format_decimal_field(hdr->ar_date, sizeof(hdr->ar_date), (long long)member->mtime);
    format_decimal_field(hdr->ar_uid, sizeof(hdr->ar_uid), (long long)member->uid);
    format_decimal_field(hdr->ar_gid, sizeof(hdr->ar_gid), (long long)member->gid);
    format_octal_field(hdr->ar_mode, sizeof(hdr->ar_mode), member->mode & 07777U);
    format_decimal_field(hdr->ar_size, sizeof(hdr->ar_size), (long long)header_size);
    memcpy(hdr->ar_fmag, ARFMAG, 2);
}

static void write_archive(const char *path, const Archive *archive)
{
    char temp_path[PATH_MAX];
    int written = snprintf(temp_path, sizeof(temp_path), "%s.tmp.XXXXXX", path);
    if (written < 0 || (size_t)written >= sizeof(temp_path)) {
        fatal_msg("archive path too long");
    }

    int fd = mkstemp(temp_path);
    if (fd < 0) {
        fatal_errno("mkstemp");
    }

    FILE *fp = fdopen(fd, "wb");
    if (!fp) {
        close(fd);
        unlink(temp_path);
        fatal_errno("fdopen");
    }

    if (fwrite(ARMAG, 1, SARMAG, fp) != SARMAG) {
        fclose(fp);
        unlink(temp_path);
        fatal_errno("write");
    }

    for (size_t i = 0; i < archive->len; i++) {
        const Member *member = &archive->items[i];
        struct ar_hdr hdr;
        fill_member_header(&hdr, member);
        if (fwrite(&hdr, 1, sizeof(hdr), fp) != sizeof(hdr)) {
            fclose(fp);
            unlink(temp_path);
            fatal_errno("write");
        }

        size_t name_len = strlen(member->name);
        size_t bytes_written = member->size;
        if (name_len > 15) {
            if (fwrite(member->name, 1, name_len, fp) != name_len) {
                fclose(fp);
                unlink(temp_path);
                fatal_errno("write");
            }
            bytes_written += name_len;
        }
        if (member->size > 0 && fwrite(member->data, 1, member->size, fp) != member->size) {
            fclose(fp);
            unlink(temp_path);
            fatal_errno("write");
        }
        if ((bytes_written & 1U) != 0 && fputc('\n', fp) == EOF) {
            fclose(fp);
            unlink(temp_path);
            fatal_errno("write");
        }
    }

    if (fflush(fp) != 0) {
        fclose(fp);
        unlink(temp_path);
        fatal_errno("fflush");
    }
    if (fclose(fp) != 0) {
        unlink(temp_path);
        fatal_errno("fclose");
    }
    if (rename(temp_path, path) != 0) {
        unlink(temp_path);
        fatal_errno("rename");
    }
}

static Member load_member_from_path(const char *path)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        fatal_errno(path);
    }

    struct stat st;
    if (fstat(fd, &st) != 0) {
        close(fd);
        fatal_errno(path);
    }
    if (!S_ISREG(st.st_mode)) {
        close(fd);
        fatal_msg("only regular files are supported");
    }

    size_t size = (size_t)st.st_size;
    unsigned char *data = xmalloc(size);
    size_t offset = 0;
    while (offset < size) {
        ssize_t got = read(fd, data + offset, size - offset);
        if (got < 0) {
            close(fd);
            free(data);
            fatal_errno(path);
        }
        if (got == 0) {
            close(fd);
            free(data);
            fatal_msg("short read");
        }
        offset += (size_t)got;
    }
    close(fd);

    Member member = {
        .name = xstrdup(base_name(path)),
        .mode = st.st_mode & 07777,
        .uid = st.st_uid,
        .gid = st.st_gid,
        .mtime = st.st_mtime,
        .data = data,
        .size = size,
    };
    return member;
}

static void print_permissions(mode_t mode)
{
    static const mode_t masks[] = {
        S_IRUSR, S_IWUSR, S_IXUSR,
        S_IRGRP, S_IWGRP, S_IXGRP,
        S_IROTH, S_IWOTH, S_IXOTH,
    };
    static const char chars[] = {'r', 'w', 'x', 'r', 'w', 'x', 'r', 'w', 'x'};

    for (size_t i = 0; i < sizeof(masks) / sizeof(masks[0]); i++) {
        putchar((mode & masks[i]) ? chars[i] : '-');
    }
}

static void print_verbose_member(const Member *member)
{
    char time_buf[64];
    struct tm tm_buf;
    if (!gmtime_r(&member->mtime, &tm_buf)) {
        memset(&tm_buf, 0, sizeof(tm_buf));
    }
    if (strftime(time_buf, sizeof(time_buf), "%b %e %H:%M %Y", &tm_buf) == 0) {
        strcpy(time_buf, "Jan  1 00:00 1970");
    }

    print_permissions(member->mode);
    printf(" %u/%u %6zu %s %s\n",
           (unsigned)member->uid,
           (unsigned)member->gid,
           member->size,
           time_buf,
           member->name);
}

static bool name_selected(const Options *opts, const char *name)
{
    if (opts->member_count == 0) {
        return true;
    }
    for (size_t i = 0; i < opts->member_count; i++) {
        if (strcmp(opts->members[i], name) == 0) {
            return true;
        }
    }
    return false;
}

static size_t insert_index_for(const Archive *archive, char pos_kind, const char *pos_name)
{
    if (!pos_kind || !pos_name) {
        return archive->len;
    }

    ssize_t pos = archive_find(archive, pos_name);
    if (pos < 0) {
        return archive->len;
    }
    return (pos_kind == 'a') ? (size_t)pos + 1 : (size_t)pos;
}

static void extract_member(const Member *member, bool verbose)
{
    int fd = open(member->name, O_WRONLY | O_CREAT | O_TRUNC, member->mode ? member->mode : 0644);
    if (fd < 0) {
        fatal_errno(member->name);
    }

    size_t offset = 0;
    while (offset < member->size) {
        ssize_t written = write(fd, member->data + offset, member->size - offset);
        if (written < 0) {
            close(fd);
            fatal_errno(member->name);
        }
        offset += (size_t)written;
    }
    if (fchmod(fd, member->mode ? member->mode : 0644) != 0) {
        close(fd);
        fatal_errno(member->name);
    }
    if (close(fd) != 0) {
        fatal_errno(member->name);
    }

    struct utimbuf times = {
        .actime = member->mtime,
        .modtime = member->mtime,
    };
    utime(member->name, &times);

    if (verbose) {
        printf("x - %s\n", member->name);
    }
}

static void do_replace(Archive *archive, const Options *opts)
{
    size_t insert_at = insert_index_for(archive, opts->pos_kind, opts->pos_name);

    for (size_t i = 0; i < opts->member_count; i++) {
        Member member = load_member_from_path(opts->members[i]);
        ssize_t existing = archive_find(archive, member.name);

        if (existing >= 0) {
            if (opts->update_only && member.mtime <= archive->items[existing].mtime) {
                member_destroy(&member);
                continue;
            }

            if (!opts->pos_kind) {
                member_destroy(&archive->items[existing]);
                archive->items[existing] = member;
                continue;
            }

            archive_remove(archive, (size_t)existing);
            if ((size_t)existing < insert_at && insert_at > 0) {
                insert_at--;
            }
        }

        archive_insert(archive, insert_at, member);
        insert_at++;
    }
}

static void do_append(Archive *archive, const Options *opts)
{
    for (size_t i = 0; i < opts->member_count; i++) {
        Member member = load_member_from_path(opts->members[i]);
        archive_push(archive, member);
    }
}

static void do_delete(Archive *archive, const Options *opts)
{
    for (size_t i = 0; i < opts->member_count; i++) {
        for (;;) {
            ssize_t index = archive_find(archive, opts->members[i]);
            if (index < 0) {
                break;
            }
            archive_remove(archive, (size_t)index);
        }
    }
}

static void do_move(Archive *archive, const Options *opts)
{
    Archive moved = {0};

    for (size_t i = 0; i < opts->member_count; i++) {
        ssize_t index = archive_find(archive, opts->members[i]);
        if (index < 0) {
            continue;
        }
        archive_push(&moved, archive_take(archive, (size_t)index));
    }

    size_t insert_at = insert_index_for(archive, opts->pos_kind, opts->pos_name);
    for (size_t i = 0; i < moved.len; i++) {
        archive_insert(archive, insert_at++, moved.items[i]);
    }
    free(moved.items);
}

static int do_list(const Archive *archive, const Options *opts)
{
    for (size_t i = 0; i < archive->len; i++) {
        const Member *member = &archive->items[i];
        if (!name_selected(opts, member->name)) {
            continue;
        }
        if (opts->verbose) {
            print_verbose_member(member);
        } else {
            printf("%s\n", member->name);
        }
    }
    return 0;
}

static int do_print(const Archive *archive, const Options *opts)
{
    for (size_t i = 0; i < archive->len; i++) {
        const Member *member = &archive->items[i];
        if (!name_selected(opts, member->name)) {
            continue;
        }
        if (member->size > 0 && fwrite(member->data, 1, member->size, stdout) != member->size) {
            fatal_errno("stdout");
        }
    }
    return 0;
}

static int do_extract(const Archive *archive, const Options *opts)
{
    for (size_t i = 0; i < archive->len; i++) {
        const Member *member = &archive->items[i];
        if (!name_selected(opts, member->name)) {
            continue;
        }
        extract_member(member, opts->verbose);
    }
    return 0;
}

static void usage(FILE *stream)
{
    fprintf(stream,
            "Usage: ar -{dmpqrtx}[abcivu] [posname] archive [members...]\n"
            "Minimal ar implementation for OSCOMP support disk overlays.\n");
}

static void parse_options(int argc, char **argv, Options *opts)
{
    memset(opts, 0, sizeof(*opts));

    if (argc == 2 && strcmp(argv[1], "--help") == 0) {
        usage(stdout);
        exit(0);
    }

    if (argc < 3) {
        usage(stderr);
        exit(1);
    }

    const char *flag_text = argv[1];
    if (strcmp(flag_text, "--help") == 0) {
        usage(stdout);
        exit(0);
    }
    if (flag_text[0] == '-') {
        flag_text++;
    }
    if (*flag_text == '\0') {
        usage(stderr);
        exit(1);
    }

    for (const char *p = flag_text; *p; p++) {
        switch (*p) {
        case 'd':
        case 'm':
        case 'p':
        case 'q':
        case 'r':
        case 't':
        case 'x':
            if (opts->op && opts->op != *p) {
                fatal_msg("multiple operations are not supported");
            }
            opts->op = *p;
            break;
        case 'a':
        case 'b':
        case 'i':
            opts->pos_kind = (*p == 'i') ? 'b' : *p;
            break;
        case 'c':
            opts->create = true;
            break;
        case 'u':
            opts->update_only = true;
            break;
        case 'v':
            opts->verbose = true;
            break;
        case 'U':
            break;
        default:
            fprintf(stderr, "ar: unsupported option -%c\n", *p);
            exit(1);
        }
    }

    if (!opts->op) {
        fatal_msg("missing operation");
    }

    int argi = 2;
    if ((opts->op == 'r' || opts->op == 'm') && opts->pos_kind) {
        if (argi >= argc) {
            fatal_msg("missing positional member name");
        }
        opts->pos_name = argv[argi++];
    }

    if (argi >= argc) {
        fatal_msg("missing archive path");
    }
    opts->archive_path = argv[argi++];
    opts->members = argv + argi;
    opts->member_count = (size_t)(argc - argi);
}

int main(int argc, char **argv)
{
    Options opts;
    parse_options(argc, argv, &opts);

    bool allow_missing = (opts.op == 'r' || opts.op == 'q');
    Archive archive;
    load_archive(opts.archive_path, &archive, allow_missing);

    switch (opts.op) {
    case 'r':
        do_replace(&archive, &opts);
        write_archive(opts.archive_path, &archive);
        break;
    case 'q':
        do_append(&archive, &opts);
        write_archive(opts.archive_path, &archive);
        break;
    case 'd':
        do_delete(&archive, &opts);
        write_archive(opts.archive_path, &archive);
        break;
    case 'm':
        do_move(&archive, &opts);
        write_archive(opts.archive_path, &archive);
        break;
    case 't':
        do_list(&archive, &opts);
        break;
    case 'p':
        do_print(&archive, &opts);
        break;
    case 'x':
        do_extract(&archive, &opts);
        break;
    default:
        archive_destroy(&archive);
        fatal_msg("unsupported operation");
    }

    archive_destroy(&archive);
    return 0;
}
