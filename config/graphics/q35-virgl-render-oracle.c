#define _GNU_SOURCE

/*
 * Exercise the render-node part of the legacy VirtGPU contract directly.
 * This deliberately does not create a 3D resource: Mesa owns command-stream
 * construction in the EGL workload.  Keeping the ABI probe to discovery,
 * capability, PRIME and sync_file primitives makes an unsupported render
 * node fail before Mesa can silently select llvmpipe.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <drm.h>
#include <drm_mode.h>
#include <virtgpu_drm.h>

static int fail(const char *reason)
{
    fprintf(stderr, "THEKERNEL_Q35_VIRGL_RENDER_READY state=FAIL reason=%s errno=%d\\n",
            reason, errno);
    return 1;
}

static int getparam(int fd, uint64_t param, uint64_t *value)
{
    struct drm_virtgpu_getparam request = { .param = param };

    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &request) < 0)
        return -1;
    *value = request.value;
    return 0;
}

static int probe_virgl_caps(int fd)
{
    uint8_t caps[4096] = { 0 };
    struct drm_virtgpu_get_caps request = {
        .cap_set_id = VIRTGPU_DRM_CAPSET_VIRGL,
        .cap_set_ver = 1,
        .addr = (uintptr_t)caps,
        .size = sizeof(caps),
    };

    return ioctl(fd, DRM_IOCTL_VIRTGPU_GET_CAPS, &request);
}

static int probe_prime(int fd)
{
    struct drm_get_cap request = { .capability = DRM_CAP_PRIME };
    if (ioctl(fd, DRM_IOCTL_GET_CAP, &request) < 0)
        return -1;
    if ((request.value & (DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT)) !=
        (DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT)) {
        errno = EOPNOTSUPP;
        return -1;
    }
    return 0;
}

static int probe_sync_file(int fd)
{
    struct drm_syncobj_create create = { .flags = DRM_SYNCOBJ_CREATE_SIGNALED };
    struct drm_syncobj_handle export_fd;
    struct drm_syncobj_handle import_fd;

    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &create) < 0)
        return -1;
    memset(&export_fd, 0, sizeof(export_fd));
    export_fd.handle = create.handle;
    export_fd.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &export_fd) < 0)
        goto destroy;
    memset(&import_fd, 0, sizeof(import_fd));
    import_fd.fd = export_fd.fd;
    import_fd.flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &import_fd) < 0) {
        close(export_fd.fd);
        goto destroy;
    }
    close(export_fd.fd);
    (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
        .handle = import_fd.handle });
    (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
        .handle = create.handle });
    return 0;

destroy:
    (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
        .handle = create.handle });
    return -1;
}

int main(void)
{
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    uint64_t value = 0;

    if (fd < 0)
        return fail("render_node");
    if (getparam(fd, VIRTGPU_PARAM_3D_FEATURES, &value) < 0 || value == 0) {
        close(fd);
        return fail("getparam_3d");
    }
    if (getparam(fd, VIRTGPU_PARAM_SUPPORTED_CAPSET_IDs, &value) < 0 ||
        !(value & (UINT64_C(1) << VIRTGPU_DRM_CAPSET_VIRGL))) {
        close(fd);
        return fail("getparam_capsets");
    }
    if (probe_virgl_caps(fd) < 0) {
        close(fd);
        return fail("get_caps_virgl");
    }
    if (probe_prime(fd) < 0) {
        close(fd);
        return fail("prime");
    }
    if (probe_sync_file(fd) < 0) {
        close(fd);
        return fail("sync_file");
    }
    close(fd);
    return 0;
}
