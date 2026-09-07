#define _GNU_SOURCE

/*
 * Exercise the render-node part of the legacy VirtGPU contract directly.
 * Mesa owns command-stream construction in the EGL workload. A small 3D
 * resource exercises PRIME export/import. Keeping the ABI probe to discovery,
 * capability, PRIME and sync_file primitives makes an unsupported render
 * node fail before Mesa can silently select llvmpipe.
 */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <poll.h>
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
    struct drm_virtgpu_getparam request = {
        .param = param,
        .value = (uintptr_t)value,
    };

    if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM, &request) < 0)
        return -1;
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
    struct drm_virtgpu_resource_create resource = {
        .target = 2, /* PIPE_TEXTURE_2D */
        .format = 1, /* PIPE_FORMAT_B8G8R8A8_UNORM */
        .bind = 2, /* VIRGL_BIND_RENDER_TARGET */
        .width = 16, .height = 16, .depth = 1, .array_size = 1,
        .size = 4096, .stride = 64,
    };
    struct drm_prime_handle exported = { .fd = -1 };
    struct drm_prime_handle imported = { 0 };
    int result = -1;
    if (ioctl(fd, DRM_IOCTL_VIRTGPU_RESOURCE_CREATE, &resource) < 0)
        return -1;
    exported.handle = resource.bo_handle;
    /* Literal x86_64 Linux O_CLOEXEC | O_RDWR guards the flag ABI. */
    exported.flags = 0x80002;
    if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &exported) < 0)
        goto prime_destroy;
    int descriptor_flags = fcntl(exported.fd, F_GETFD);
    if (descriptor_flags < 0)
        goto prime_destroy;
    if (!(descriptor_flags & FD_CLOEXEC)) {
        errno = EIO;
        goto prime_destroy;
    }
    imported.fd = exported.fd;
    if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &imported) < 0)
        goto prime_destroy;
    if (!imported.handle) {
        errno = EIO;
        goto prime_destroy;
    }
    result = 0;
prime_destroy:;
    int saved_errno = errno;
    if (imported.handle && imported.handle != resource.bo_handle)
        (void)ioctl(fd, DRM_IOCTL_GEM_CLOSE, &(struct drm_gem_close) {
            .handle = imported.handle });
    if (exported.fd >= 0)
        close(exported.fd);
    (void)ioctl(fd, DRM_IOCTL_GEM_CLOSE, &(struct drm_gem_close) {
        .handle = resource.bo_handle });
    errno = saved_errno;
    return result;
}

static int probe_sync_file(int fd)
{
    struct drm_syncobj_create create = { .flags = DRM_SYNCOBJ_CREATE_SIGNALED };
    struct drm_syncobj_create destination = { 0 };
    struct drm_syncobj_handle export_fd = { .fd = -1 };
    struct drm_syncobj_handle imported_fd = { .fd = -1 };
    int result = -1;

    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &create) < 0)
        return -1;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &destination) < 0)
        goto destroy;
    export_fd.handle = create.handle;
    export_fd.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &export_fd) < 0)
        goto destroy;
    /* IMPORT_SYNC_FILE replaces the fence in an existing destination. */
    struct drm_syncobj_handle import_fd = {
        .handle = destination.handle,
        .fd = export_fd.fd,
        .flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
    };
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &import_fd) < 0)
        goto destroy;
    imported_fd.handle = destination.handle;
    imported_fd.flags = DRM_SYNCOBJ_HANDLE_TO_FD_FLAGS_EXPORT_SYNC_FILE;
    if (ioctl(fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &imported_fd) < 0)
        goto destroy;
    struct pollfd ready = { .fd = imported_fd.fd, .events = POLLIN };
    if (poll(&ready, 1, 0) != 1 || !(ready.revents & POLLIN) ||
        (ready.revents & (POLLERR | POLLNVAL))) {
        errno = EIO;
        goto destroy;
    }
    result = 0;

destroy:;
    int saved_errno = errno;
    if (imported_fd.fd >= 0)
        close(imported_fd.fd);
    if (export_fd.fd >= 0)
        close(export_fd.fd);
    if (destination.handle)
        (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
            .handle = destination.handle });
    (void)ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &(struct drm_syncobj_destroy) {
        .handle = create.handle });
    errno = saved_errno;
    return result;
}

int main(void)
{
    int fd = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    uint64_t value = 0;

    if (fd < 0)
        return fail("render_node");
    struct drm_mode_card_res kms = { 0 };
    errno = 0;
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &kms) == 0 || errno != ENOTTY) {
        close(fd);
        return fail("render_node_kms_isolation");
    }
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
