#define _DEFAULT_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/input.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

static void result(FILE *output, const char *state, unsigned long sequence, const char *detail) {
    fprintf(output, "%lu %s %s\n", sequence, state, detail);
    fflush(output);
}

static int drm_usable(int fd) {
    struct drm_mode_card_res resources = {0};
    return ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &resources) == 0;
}

static int input_usable(int fd) {
    int version = 0;
    return ioctl(fd, EVIOCGVERSION, &version) == 0;
}

static int revoked(int card, int input, char *detail, size_t detail_size) {
    struct pollfd fds[] = {
        {.fd = card, .events = POLLIN | POLLPRI},
        {.fd = input, .events = POLLIN | POLLPRI},
    };
    int poll_result = poll(fds, 2, 0);
    int card_errno = 0, input_errno = 0, read_errno = 0;
    char byte;
    if (!drm_usable(card)) card_errno = errno;
    if (!input_usable(input)) input_errno = errno;
    if (read(input, &byte, 1) < 0) read_errno = errno;
    snprintf(detail, detail_size, "poll=%x/%x drm=%d input=%d read=%d",
             poll_result < 0 ? 0 : fds[0].revents,
             poll_result < 0 ? 0 : fds[1].revents,
             card_errno, input_errno, read_errno);
    int card_revoked = (poll_result >= 0 && (fds[0].revents & (POLLHUP | POLLERR))) ||
                       card_errno == ENODEV;
    int input_revoked = (poll_result >= 0 && (fds[1].revents & (POLLHUP | POLLERR))) ||
                        input_errno == ENODEV || read_errno == ENODEV;
    return card_revoked && input_revoked;
}

int main(int argc, char **argv) {
    char command_path[512], result_path[512], line[128], previous[128] = "";
    const char *state_dir;
    int card, input;
    if (argc != 3) return 2;
    state_dir = argv[1];
    if (snprintf(command_path, sizeof(command_path), "%s/lease.command", state_dir) >= (int)sizeof(command_path) ||
        snprintf(result_path, sizeof(result_path), "%s/lease.result", state_dir) >= (int)sizeof(result_path)) return 2;
    card = open("/dev/dri/card0", O_RDWR | O_CLOEXEC | O_NONBLOCK);
    input = open(argv[2], O_RDONLY | O_CLOEXEC | O_NONBLOCK);
    if (card < 0 || input < 0 || !drm_usable(card) || !input_usable(input)) {
        result(stdout, "initial-failed", 0, "open-or-ioctl");
        return 1;
    }
    result(stdout, "ready", 0, "drm-and-input-usable");
    for (;;) {
        FILE *command = fopen(command_path, "r");
        unsigned long sequence = 0;
        char operation[32] = "";
        if (command != NULL) {
            if (fgets(line, sizeof(line), command) != NULL && strcmp(line, previous) != 0 &&
                sscanf(line, "%lu %31s", &sequence, operation) == 2) {
                char detail[128];
                FILE *output;
                snprintf(previous, sizeof(previous), "%s", line);
                output = fopen(result_path, "w");
                if (output == NULL) return 1;
                if (strcmp(operation, "active") == 0) {
                    result(output, drm_usable(card) && input_usable(input) ? "active-ok" : "active-failed", sequence,
                           "drm-and-input-ioctl");
                } else if (strcmp(operation, "revoked") == 0) {
                    result(output, revoked(card, input, detail, sizeof(detail)) ? "revoked" : "not-revoked", sequence, detail);
                } else {
                    result(output, "invalid-command", sequence, operation);
                }
                fclose(output);
            }
            fclose(command);
        }
        usleep(25 * 1000);
    }
}
