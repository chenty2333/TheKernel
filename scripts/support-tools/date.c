#define _GNU_SOURCE

#include <errno.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static void usage(FILE *stream)
{
    fprintf(stream,
            "Usage: date [-u] [+FORMAT]\n"
            "       date [-u] -d STRING [+FORMAT]\n"
            "       date [-u] --date=STRING [+FORMAT]\n");
}

static void die_usage(void)
{
    usage(stderr);
    exit(1);
}

static void die_parse(const char *value)
{
    fprintf(stderr, "date: invalid date '%s'\n", value);
    exit(1);
}

static bool parse_epoch(const char *value, time_t *out)
{
    char *end = NULL;
    errno = 0;
    long long parsed = strtoll(value, &end, 10);
    if (errno != 0 || !end || *end != '\0') {
        return false;
    }
    *out = (time_t)parsed;
    return true;
}

static bool parse_common_datetime(const char *value, bool utc, time_t *out)
{
    static const char *formats[] = {
        "%Y%m%d%H%M.%S",
        "%Y%m%d%H%M%S",
        "%Y%m%d%H%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%d",
    };

    for (size_t i = 0; i < sizeof(formats) / sizeof(formats[0]); i++) {
        struct tm tm_buf;
        memset(&tm_buf, 0, sizeof(tm_buf));
        tm_buf.tm_isdst = -1;

        char *end = strptime(value, formats[i], &tm_buf);
        if (!end || *end != '\0') {
            continue;
        }

        time_t when = utc ? timegm(&tm_buf) : mktime(&tm_buf);
        if (when == (time_t)-1) {
            continue;
        }
        *out = when;
        return true;
    }

    return false;
}

static time_t parse_date_arg(const char *value, bool utc)
{
    time_t now = time(NULL);

    if (strcmp(value, "next day") == 0 || strcmp(value, "tomorrow") == 0) {
        return now + 24 * 60 * 60;
    }
    if (value[0] == '@') {
        time_t parsed;
        if (parse_epoch(value + 1, &parsed)) {
            return parsed;
        }
    }
    if (parse_common_datetime(value, utc, &now)) {
        return now;
    }

    die_parse(value);
    return (time_t)0;
}

static void append_text(char *dst, size_t *used, size_t cap, const char *src)
{
    size_t len = strlen(src);
    if (*used + len + 1 >= cap) {
        fprintf(stderr, "date: formatted output too long\n");
        exit(1);
    }
    memcpy(dst + *used, src, len);
    *used += len;
    dst[*used] = '\0';
}

static void append_char(char *dst, size_t *used, size_t cap, char ch)
{
    if (*used + 2 >= cap) {
        fprintf(stderr, "date: formatted output too long\n");
        exit(1);
    }
    dst[*used] = ch;
    (*used)++;
    dst[*used] = '\0';
}

static void format_output(const char *format, time_t when, bool utc)
{
    char output[4096];
    size_t used = 0;
    output[0] = '\0';

    struct tm tm_buf;
    if (utc) {
        gmtime_r(&when, &tm_buf);
    } else {
        localtime_r(&when, &tm_buf);
    }

    for (size_t i = 0; format[i] != '\0';) {
        if (format[i] != '%') {
            append_char(output, &used, sizeof(output), format[i]);
            i++;
            continue;
        }

        if (format[i + 1] == '\0') {
            append_char(output, &used, sizeof(output), '%');
            break;
        }

        if (format[i + 1] == '%') {
            append_char(output, &used, sizeof(output), '%');
            i += 2;
            continue;
        }

        if (format[i + 1] == 's') {
            char epoch_buf[64];
            snprintf(epoch_buf, sizeof(epoch_buf), "%lld", (long long)when);
            append_text(output, &used, sizeof(output), epoch_buf);
            i += 2;
            continue;
        }

        char piece_fmt[3] = {'%', format[i + 1], '\0'};
        char piece_out[256];
        if (strftime(piece_out, sizeof(piece_out), piece_fmt, &tm_buf) == 0) {
            fprintf(stderr, "date: unsupported format '%s'\n", piece_fmt);
            exit(1);
        }
        append_text(output, &used, sizeof(output), piece_out);
        i += 2;
    }

    puts(output);
}

int main(int argc, char **argv)
{
    bool utc = false;
    const char *date_arg = NULL;
    const char *format = NULL;

    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];

        if (strcmp(arg, "--help") == 0) {
            usage(stdout);
            return 0;
        }
        if (strcmp(arg, "-u") == 0 || strcmp(arg, "--utc") == 0) {
            utc = true;
            continue;
        }
        if (strcmp(arg, "-d") == 0 || strcmp(arg, "--date") == 0) {
            if (++i >= argc) {
                die_usage();
            }
            date_arg = argv[i];
            continue;
        }
        if (strncmp(arg, "--date=", 7) == 0) {
            date_arg = arg + 7;
            continue;
        }
        if (arg[0] == '+') {
            format = arg + 1;
            continue;
        }

        die_usage();
    }

    time_t when = date_arg ? parse_date_arg(date_arg, utc) : time(NULL);
    if (!format) {
        format = utc ? "%a %b %e %H:%M:%S UTC %Y" : "%a %b %e %H:%M:%S %Z %Y";
    }

    format_output(format, when, utc);
    return 0;
}
