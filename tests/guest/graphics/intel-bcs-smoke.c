/* SPDX-License-Identifier: MIT
 * Copyright 2026 TheKernel contributors.
 * Native opt-in acceptance, not part of automatic QEMU success counting.
 * This program never starts a GPU implicitly: a user must explicitly choose
 * a DRM node and confirm the experimental driver with --execute.
 */
#define _GNU_SOURCE
#include <drm.h>
#include <i915_drm.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define BYTES 16384u
static int call(int fd, unsigned long command, void *arg) {
    if (ioctl(fd, command, arg) == 0) return 0;
    fprintf(stderr, "INTEL_BCS_FAIL ioctl=0x%lx errno=%d\n", command, errno);
    return -1;
}
int main(int argc, char **argv) {
    if (argc != 3 || strcmp(argv[1], "--execute") != 0) {
        fprintf(stderr, "usage: intel-bcs-smoke --execute /dev/dri/renderD128\n");
        return 2;
    }
    int fd = open(argv[2], O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("DRM open; not tested"); return 1; }
    int result = 1;
    uint32_t handles[3] = {0}, sync = 0;
    unsigned char source[BYTES], output[BYTES];
    for (unsigned i=0;i<BYTES;i++) source[i]=(unsigned char)((i*29u)^(i>>8)^0x73u);
    /* Only the validated linear fast-copy + END shape; no user LRI/register
     * programming. The native driver reconstructs its own complete batch. */
    const uint32_t batch[11] = {0x50800008,0x03000100,0,0x00400040,0x20000,0,0,256,0x10000,0,0x05000000};
    for (unsigned i=0;i<3;i++) {
        struct drm_i915_gem_create request={.size=BYTES};
        if (call(fd,DRM_IOCTL_I915_GEM_CREATE,&request)) goto done;
        handles[i]=request.handle;
    }
    struct drm_i915_gem_pwrite upload={.handle=handles[0],.size=BYTES,.data_ptr=(uintptr_t)source};
    if (call(fd,DRM_IOCTL_I915_GEM_PWRITE,&upload)) goto done;
    upload=(struct drm_i915_gem_pwrite){.handle=handles[2],.size=sizeof(batch),.data_ptr=(uintptr_t)batch};
    if (call(fd,DRM_IOCTL_I915_GEM_PWRITE,&upload)) goto done;
    struct drm_syncobj_create sync_create={0};
    if (call(fd,DRM_IOCTL_SYNCOBJ_CREATE,&sync_create)) goto done;
    sync=sync_create.handle;
    struct drm_i915_gem_exec_object2 objects[3]={0};
    for(unsigned i=0;i<3;i++) {
        objects[i].handle=handles[i];objects[i].offset=0x10000u*(i+1u);
        objects[i].flags=EXEC_OBJECT_PINNED|EXEC_OBJECT_SUPPORTS_48B_ADDRESS|(i==1?EXEC_OBJECT_WRITE:0);
    }
    struct drm_i915_gem_exec_fence fence={.handle=sync,.flags=I915_EXEC_FENCE_SIGNAL};
    struct drm_i915_gem_execbuffer2 exec={.buffers_ptr=(uintptr_t)objects,.buffer_count=3,.batch_len=sizeof(batch),.flags=I915_EXEC_BLT|I915_EXEC_NO_RELOC|I915_EXEC_FENCE_ARRAY,.num_cliprects=1,.cliprects_ptr=(uintptr_t)&fence};
    if (call(fd,DRM_IOCTL_I915_GEM_EXECBUFFER2,&exec)) goto done;
    struct drm_i915_gem_wait wait={.bo_handle=handles[1],.timeout_ns=1000000000};
    if (call(fd,DRM_IOCTL_I915_GEM_WAIT,&wait)) goto done;
    struct drm_i915_gem_pread readback={.handle=handles[1],.size=BYTES,.data_ptr=(uintptr_t)output};
    if (call(fd,DRM_IOCTL_I915_GEM_PREAD,&readback)) goto done;
    for(unsigned i=0;i<BYTES;i++) if(output[i]!=source[i]) {
        fprintf(stderr,"INTEL_BCS_FAIL byte=%u actual=%u expected=%u\n",i,output[i],source[i]);goto done;
    }
    struct drm_syncobj_wait sync_wait={.handles=(uintptr_t)&sync,.count_handles=1,.timeout_nsec=0};
    if(call(fd,DRM_IOCTL_SYNCOBJ_WAIT,&sync_wait)) goto done;
    /* Prove mappings retain the same GEM storage after handle close. */
    struct drm_i915_gem_mmap_offset map={.handle=handles[1],.flags=I915_MMAP_OFFSET_WB};
    if (call(fd,DRM_IOCTL_I915_GEM_MMAP_OFFSET,&map)) goto done;
    void *address=mmap(NULL,BYTES,PROT_READ,MAP_SHARED,fd,(off_t)map.offset);
    if (address==MAP_FAILED){perror("GEM mmap");goto done;}
    struct drm_gem_close close_request={.handle=handles[1]};
    if (call(fd,DRM_IOCTL_GEM_CLOSE,&close_request)){munmap(address,BYTES);goto done;}
    handles[1]=0;
    if(memcmp(address,source,BYTES)){fprintf(stderr,"INTEL_BCS_FAIL mmap/close bytes\n");munmap(address,BYTES);goto done;}
    munmap(address,BYTES);
    puts("INTEL_BCS_USER_BYTES_VERIFIED bytes=16384 GEM/exec/sync/mmap; not RCS/Mesa rendering");
    result=0;
done:
    if(sync){struct drm_syncobj_destroy request={.handle=sync};ioctl(fd,DRM_IOCTL_SYNCOBJ_DESTROY,&request);}
    for(unsigned i=0;i<3;i++) if(handles[i]){struct drm_gem_close request={.handle=handles[i]};ioctl(fd,DRM_IOCTL_GEM_CLOSE,&request);}
    close(fd);return result;
}
