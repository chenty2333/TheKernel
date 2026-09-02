/*
 * Deliberately small KMS fault injector for the q35 benchmark image.
 *
 * It runs only after Weston has released DRM master.  The helper selects one
 * connected connector and a compatible CRTC, creates two scanout-capable dumb
 * buffers for two distinct advertised modes, and drives both modes through
 * DRM_IOCTL_MODE_SETCRTC.  It then disables that CRTC and releases all KMS
 * objects.  The caller restarts Weston afterwards, which owns the recovery
 * modeset and repaint; retaining a Weston's framebuffer after its FD closes is
 * neither possible nor a meaningful restore operation.
 */
#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>

/* libdrm's xf86drmMode.h calls this DRM_MODE_CONNECTED, but this helper uses
 * only the kernel UAPI headers, where connector status is the wire value 1. */
enum { DRM_CONNECTOR_STATUS_CONNECTED = 1 };

struct mode_target {
    __u32 connector_id;
    __u32 crtc_id;
    struct drm_mode_modeinfo first;
    struct drm_mode_modeinfo second;
};

static int fail(const char *operation) {
    fprintf(stderr, "q35-drm-modeset-restore: %s: %s\n", operation, strerror(errno));
    return -1;
}

static int get_resources(int fd, struct drm_mode_card_res *resources, __u32 **crtcs,
                         __u32 **connectors) {
    memset(resources, 0, sizeof(*resources));
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, resources) != 0) return fail("GETRESOURCES");
    if (resources->count_crtcs == 0 || resources->count_connectors == 0) {
        errno = ENODEV;
        return fail("no CRTC or connector");
    }
    *crtcs = calloc(resources->count_crtcs, sizeof(**crtcs));
    *connectors = calloc(resources->count_connectors, sizeof(**connectors));
    if (*crtcs == NULL || *connectors == NULL) return fail("allocate resources");
    resources->crtc_id_ptr = (uintptr_t)*crtcs;
    resources->connector_id_ptr = (uintptr_t)*connectors;
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, resources) != 0) return fail("GETRESOURCES list");
    return 0;
}

static int crtc_for_encoder(int fd, const struct drm_mode_card_res *resources, __u32 encoder_id,
                            __u32 *crtc_id) {
    struct drm_mode_get_encoder encoder = { .encoder_id = encoder_id };
    if (ioctl(fd, DRM_IOCTL_MODE_GETENCODER, &encoder) != 0) return -1;
    if (encoder.crtc_id != 0) {
        *crtc_id = encoder.crtc_id;
        return 0;
    }
    for (__u32 index = 0; index < resources->count_crtcs && index < 32; ++index) {
        if ((encoder.possible_crtcs & (UINT32_C(1) << index)) != 0) {
            *crtc_id = ((__u32 *)(uintptr_t)resources->crtc_id_ptr)[index];
            return 0;
        }
    }
    errno = ENODEV;
    return -1;
}

static int choose_target(int fd, const struct drm_mode_card_res *resources, __u32 *connectors,
                         struct mode_target *target) {
    for (__u32 index = 0; index < resources->count_connectors; ++index) {
        struct drm_mode_get_connector connector = { .connector_id = connectors[index] };
        if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &connector) != 0 ||
            connector.connection != DRM_CONNECTOR_STATUS_CONNECTED || connector.count_modes < 2) continue;
        struct drm_mode_modeinfo *modes = calloc(connector.count_modes, sizeof(*modes));
        __u32 *encoders = calloc(connector.count_encoders, sizeof(*encoders));
        if (modes == NULL || encoders == NULL) {
            free(modes);
            free(encoders);
            return fail("allocate connector");
        }
        connector.modes_ptr = (uintptr_t)modes;
        connector.encoders_ptr = (uintptr_t)encoders;
        if (ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR, &connector) == 0) {
            __u32 crtc_id = 0;
            if (connector.encoder_id != 0)
                (void)crtc_for_encoder(fd, resources, connector.encoder_id, &crtc_id);
            for (__u32 encoder = 0; crtc_id == 0 && encoder < connector.count_encoders; ++encoder)
                (void)crtc_for_encoder(fd, resources, encoders[encoder], &crtc_id);
            if (crtc_id != 0) {
                for (__u32 candidate = 1; candidate < connector.count_modes; ++candidate) {
                    if (modes[0].hdisplay != modes[candidate].hdisplay ||
                        modes[0].vdisplay != modes[candidate].vdisplay ||
                        modes[0].clock != modes[candidate].clock) {
                        target->connector_id = connector.connector_id;
                        target->crtc_id = crtc_id;
                        target->first = modes[0];
                        target->second = modes[candidate];
                        free(modes);
                        free(encoders);
                        return 0;
                    }
                }
            }
        }
        free(modes);
        free(encoders);
    }
    errno = ENODEV;
    return fail("connected connector with two distinct modes");
}

static int create_framebuffer(int fd, const struct drm_mode_modeinfo *mode, __u32 *handle,
                              __u32 *fb_id) {
    struct drm_mode_create_dumb dumb = {
        .width = mode->hdisplay,
        .height = mode->vdisplay,
        .bpp = 32,
    };
    struct drm_mode_fb_cmd fb = { 0 };
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) != 0) return fail("CREATE_DUMB");
    fb.width = dumb.width;
    fb.height = dumb.height;
    fb.pitch = dumb.pitch;
    fb.bpp = 32;
    fb.depth = 24;
    fb.handle = dumb.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_ADDFB, &fb) != 0) {
        struct drm_mode_destroy_dumb destroy = { .handle = dumb.handle };
        (void)ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
        return fail("ADDFB");
    }
    *handle = dumb.handle;
    *fb_id = fb.fb_id;
    return 0;
}

static void destroy_framebuffer(int fd, __u32 handle, __u32 fb_id) {
    if (fb_id != 0) (void)ioctl(fd, DRM_IOCTL_MODE_RMFB, &fb_id);
    if (handle != 0) {
        struct drm_mode_destroy_dumb destroy = { .handle = handle };
        (void)ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy);
    }
}

static int modeset(int fd, const struct mode_target *target, __u32 fb_id,
                   const struct drm_mode_modeinfo *mode) {
    struct drm_mode_crtc crtc = {
        .crtc_id = target->crtc_id,
        .fb_id = fb_id,
        .set_connectors_ptr = (uintptr_t)&target->connector_id,
        .count_connectors = 1,
        .mode_valid = 1,
        .mode = *mode,
    };
    if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &crtc) != 0) return fail("SETCRTC");
    struct timespec interval = { .tv_nsec = 50000000 };
    (void)nanosleep(&interval, NULL);
    return 0;
}

int main(void) {
    int status = 1;
    int fd = -1;
    __u32 *crtcs = NULL;
    __u32 *connectors = NULL;
    __u32 first_handle = 0, first_fb = 0, second_handle = 0, second_fb = 0;
    struct drm_mode_card_res resources;
    struct mode_target target;

    fd = open("/dev/dri/card0", O_RDWR | O_CLOEXEC);
    if (fd < 0) goto out;
    if (ioctl(fd, DRM_IOCTL_SET_MASTER, 0) != 0) goto out;
    if (get_resources(fd, &resources, &crtcs, &connectors) != 0) goto out;
    if (choose_target(fd, &resources, connectors, &target) != 0) goto out;
    if (create_framebuffer(fd, &target.first, &first_handle, &first_fb) != 0) goto out;
    if (create_framebuffer(fd, &target.second, &second_handle, &second_fb) != 0) goto out;
    if (modeset(fd, &target, first_fb, &target.first) != 0) goto out;
    if (modeset(fd, &target, second_fb, &target.second) != 0) goto out;
    {
        struct drm_mode_crtc disable = { .crtc_id = target.crtc_id };
        if (ioctl(fd, DRM_IOCTL_MODE_SETCRTC, &disable) != 0) goto out;
    }
    printf("THEKERNEL_GRAPHICS_MODESET connector=%u crtc=%u first=%ux%u second=%ux%u\n",
           target.connector_id, target.crtc_id, target.first.hdisplay, target.first.vdisplay,
           target.second.hdisplay, target.second.vdisplay);
    status = 0;
out:
    destroy_framebuffer(fd, second_handle, second_fb);
    destroy_framebuffer(fd, first_handle, first_fb);
    free(connectors);
    free(crtcs);
    if (fd >= 0) close(fd);
    return status;
}
