/* SPDX-License-Identifier: MIT
 * Copyright 2026 TheKernel contributors.
 * Native opt-in acceptance, not part of automatic QEMU success counting.
 * This program never starts a GPU implicitly: a user must explicitly choose
 * a DRM node and confirm the experimental driver with --execute or --rcs-execute.
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

#include "intel-rcs-page.h"
#define BYTES 16384u
static int call(int fd, unsigned long command, void *arg) {
    if (ioctl(fd, command, arg) == 0) return 0;
    fprintf(stderr, "INTEL_BCS_FAIL ioctl=0x%lx errno=%d\n", command, errno);
    return -1;
}
int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[1], "--execute") != 0 && strcmp(argv[1],"--rcs-execute")!=0)) {
        fprintf(stderr, "usage: intel-bcs-smoke {--execute|--rcs-execute} /dev/dri/renderD128\n");
        return 2;
    }
    int render = strcmp(argv[1],"--rcs-execute")==0;
    int fd = open(argv[2], O_RDWR | O_CLOEXEC);
    if (fd < 0) { perror("DRM open; not tested"); return 1; }
    int result = 1;
    uint32_t handles[3] = {0}, sync = 0, context = 0;
    int chipset = 0;
    drm_i915_getparam_t chipset_query = {.param=I915_PARAM_CHIPSET_ID, .value=&chipset};
    if (call(fd, DRM_IOCTL_I915_GETPARAM, &chipset_query)) goto done;
    if (chipset!=0x46d0) {fprintf(stderr,"INTEL_FAIL wrong chipset=0x%x\n",chipset);goto done;}
    struct drm_i915_query_item engine_item = {.query_id=DRM_I915_QUERY_ENGINE_INFO};
    struct drm_i915_query engine_query = {.num_items=1, .items_ptr=(uintptr_t)&engine_item};
    if (call(fd, DRM_IOCTL_I915_QUERY, &engine_query)) goto done;
    uint64_t engine_storage[128]={0};
    if (engine_item.length<16 || engine_item.length>(int)sizeof(engine_storage)) {fprintf(stderr,"INTEL_FAIL engine query length=%d\n",engine_item.length);goto done;}
    engine_item.data_ptr=(uintptr_t)engine_storage;
    if (call(fd, DRM_IOCTL_I915_QUERY, &engine_query)) goto done;
    struct drm_i915_query_engine_info *engines=(void*)engine_storage;
    if (engine_item.length<16 || engines->num_engines>(sizeof(engine_storage)-16)/sizeof(struct drm_i915_engine_info) || 16+engines->num_engines*sizeof(struct drm_i915_engine_info)>(unsigned)engine_item.length) goto done;
    int found=0;
    for (unsigned i=0;i<engines->num_engines;i++) if(engines->engines[i].engine.engine_class==(render?I915_ENGINE_CLASS_RENDER:I915_ENGINE_CLASS_COPY) && engines->engines[i].engine.engine_instance==0)found=1;
    if(!found){fprintf(stderr,"INTEL_FAIL requested engine not available\n");goto done;}
    struct drm_i915_gem_context_create create_context={0};
    if(call(fd,DRM_IOCTL_I915_GEM_CONTEXT_CREATE,&create_context))goto done;
    context=create_context.ctx_id;
    if(!context){fprintf(stderr,"INTEL_FAIL default context returned by CREATE\n");goto done;}
    unsigned char source[BYTES], output[BYTES];
    unsigned char guarded[24576];
    for (unsigned i=0;i<BYTES;i++) source[i]=(unsigned char)((i*29u)^(i>>8)^0x73u);
    if(render) for(unsigned i=3;i<BYTES;i+=4)source[i]=255;
    /* Only the validated linear fast-copy + END shape; no user LRI/register
     * programming. The native driver reconstructs its own complete batch. */
    const uint32_t batch[11] = {0x50800008,0x03000100,0,0x00400040,0x20000,0,0,256,0x10000,0,0x05000000};
    for (unsigned i=0;i<3;i++) {
        struct drm_i915_gem_create request={.size=render?24576:BYTES};
        if (call(fd,DRM_IOCTL_I915_GEM_CREATE,&request)) goto done;
        handles[i]=request.handle;
    }
    struct drm_i915_gem_pwrite upload={.handle=handles[0],.size=BYTES,.data_ptr=(uintptr_t)source};
    if(render){ memset(guarded,0xa5,sizeof(guarded));memcpy(guarded+4096,source,BYTES);upload.size=sizeof(guarded);upload.data_ptr=(uintptr_t)guarded;}
    if (call(fd,DRM_IOCTL_I915_GEM_PWRITE,&upload)) goto done;
    if(render){memset(guarded,0x5a,sizeof(guarded));memset(guarded+4096,0,BYTES);struct drm_i915_gem_pwrite destination={.handle=handles[1],.size=sizeof(guarded),.data_ptr=(uintptr_t)guarded};if(call(fd,DRM_IOCTL_I915_GEM_PWRITE,&destination))goto done;}
    upload=(struct drm_i915_gem_pwrite){.handle=handles[2],.size=sizeof(batch),.data_ptr=(uintptr_t)batch};
    if(render){upload.size=sizeof(rcs_page);upload.data_ptr=(uintptr_t)rcs_page;}
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
    uint64_t point=1;
    struct drm_i915_gem_execbuffer_ext_timeline_fences timeline={.base={.name=DRM_I915_GEM_EXECBUFFER_EXT_TIMELINE_FENCES},.fence_count=1,.handles_ptr=(uintptr_t)&fence,.values_ptr=(uintptr_t)&point};
    struct drm_i915_gem_execbuffer2 exec={.buffers_ptr=(uintptr_t)objects,.buffer_count=3,.batch_len=render?1160:sizeof(batch),.rsvd1=context,.flags=(render?I915_EXEC_RENDER:I915_EXEC_BLT)|I915_EXEC_NO_RELOC|I915_EXEC_USE_EXTENSIONS,.cliprects_ptr=(uintptr_t)&timeline};
    if (call(fd,DRM_IOCTL_I915_GEM_EXECBUFFER2,&exec)) goto done;
    struct drm_i915_gem_wait wait={.bo_handle=handles[1],.timeout_ns=1000000000};
    if (call(fd,DRM_IOCTL_I915_GEM_WAIT,&wait)) goto done;
    struct drm_i915_gem_pread readback={.handle=handles[1],.offset=render?4096:0,.size=BYTES,.data_ptr=(uintptr_t)output};
    if (call(fd,DRM_IOCTL_I915_GEM_PREAD,&readback)) goto done;
    for(unsigned i=0;i<BYTES;i++) if(output[i]!=source[i]) {
        fprintf(stderr,"INTEL_BCS_FAIL byte=%u actual=%u expected=%u\n",i,output[i],source[i]);goto done;
    }
    struct drm_syncobj_timeline_wait sync_wait={.handles=(uintptr_t)&sync,.points=(uintptr_t)&point,.count_handles=1,.timeout_nsec=0};
    if(call(fd,DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT,&sync_wait)) goto done;
    if(render){struct drm_i915_gem_pread guards={.handle=handles[1],.size=sizeof(guarded),.data_ptr=(uintptr_t)guarded};if(call(fd,DRM_IOCTL_I915_GEM_PREAD,&guards))goto done;for(unsigned i=0;i<sizeof(guarded);i++)if((i<4096||i>=4096+BYTES)&&guarded[i]!=0x5a){fprintf(stderr,"INTEL_RCS_FAIL guard=%u\n",i);goto done;}}
    if (render) {
        struct drm_i915_gem_pread unchanged = {.handle=handles[0], .size=sizeof(guarded), .data_ptr=(uintptr_t)guarded};
        if (call(fd, DRM_IOCTL_I915_GEM_PREAD, &unchanged)) goto done;
        for (unsigned i=0; i<sizeof(guarded); i++) {
            unsigned char expected=(i<4096 || i>=4096+BYTES)?0xa5:source[i-4096];
            if (guarded[i]!=expected) {fprintf(stderr,"INTEL_RCS_FAIL source=%u\n",i);goto done;}
        }
    }
    /* Prove mappings retain the same GEM storage after handle close. */
    struct drm_i915_gem_mmap_offset map={.handle=handles[1],.flags=I915_MMAP_OFFSET_WB};
    if (call(fd,DRM_IOCTL_I915_GEM_MMAP_OFFSET,&map)) goto done;
    size_t mapped_size=render?24576:BYTES;
    void *address=mmap(NULL,mapped_size,PROT_READ,MAP_SHARED,fd,(off_t)map.offset);
    if (address==MAP_FAILED){perror("GEM mmap");goto done;}
    struct drm_gem_close close_request={.handle=handles[1]};
    if (call(fd,DRM_IOCTL_GEM_CLOSE,&close_request)){munmap(address,mapped_size);goto done;}
    handles[1]=0;
    if(memcmp((unsigned char*)address+(render?4096:0),source,BYTES)){fprintf(stderr,"INTEL_BCS_FAIL mmap/close bytes\n");munmap(address,mapped_size);goto done;}
    munmap(address,mapped_size);
    puts(render?"INTEL_RCS_USER_BYTES_VERIFIED bytes=16384 shader/GEM/sync/mmap; not Mesa acceptance":"INTEL_BCS_USER_BYTES_VERIFIED bytes=16384 GEM/exec/sync/mmap; not RCS/Mesa rendering");
    result=0;
done:
    if(context){struct drm_i915_gem_context_destroy request={.ctx_id=context};ioctl(fd,DRM_IOCTL_I915_GEM_CONTEXT_DESTROY,&request);}
    if(sync){struct drm_syncobj_destroy request={.handle=sync};ioctl(fd,DRM_IOCTL_SYNCOBJ_DESTROY,&request);}
    for(unsigned i=0;i<3;i++) if(handles[i]){struct drm_gem_close request={.handle=handles[i]};ioctl(fd,DRM_IOCTL_GEM_CLOSE,&request);}
    close(fd);return result;
}
