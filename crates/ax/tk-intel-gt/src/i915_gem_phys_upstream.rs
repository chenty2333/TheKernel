// SPDX-License-Identifier: MIT
// Copyright © 2014-2016 Intel Corporation.
//! Linux 7.2.3 gem/i915_gem_phys.c, all nine functions in source order.
#![allow(unsafe_code,unsafe_op_in_unsafe_fn)]
use core::ffi::{c_int,c_void};
use crate::{i915_gem_object_types_upstream::DrmI915GemObject,intel_context_upstream::SgTable,i915_gem_pages_upstream::{Scatterlist,sg_page,sg_alloc_table,__i915_gem_object_set_pages,__i915_gem_object_unset_pages},i915_gem_object_header_upstream::{assert_object_held,__i915_gem_object_pin_pages,i915_gem_object_has_pinned_pages,i915_gem_object_has_tiling_quirk},i915_gem_object_upstream::i915_gem_object_has_struct_page,i915_gem_tiling_upstream::i915_gem_object_needs_bit17_swizzle,i915_gem_region_upstream::i915_gem_object_release_memory_region,i915_gem_core_upstream::{i915_gem_object_wait,i915_gem_object_unbind,i915_gem_object_frontbuffer_invalidate,i915_gem_object_frontbuffer_flush},intel_engine_api_upstream::drm_clflush_virt_range,intel_gt_api_upstream::intel_gt_chipset_flush,linux::{gem::drm_device_device,dma::{dma_alloc_coherent,dma_free_coherent},i915::{to_gt,to_i915},memory::{kmalloc_obj,kfree},highmem::{kmap_local_page,kunmap_local},shmem::{File,shmem_read_mapping_page,put_page,page_folio,folio_mark_dirty,mark_page_accessed},mm_native::{copy_from_user,copy_to_user}},i915_gem_shmem_upstream::{DrmI915GemPread,DrmI915GemPwrite,__i915_gem_object_release_shmem,i915_gem_object_put_pages_shmem,i915_gem_object_is_shmem},i915_gem_userptr_upstream::sg_free_table,linux_config::{GFP_KERNEL,MAX_SCHEDULE_TIMEOUT,ENOMEM,EINVAL,EFAULT,EBUSY,E2BIG}};
unsafe fn file(obj:*mut DrmI915GemObject)->*mut File {(*obj).base.base.filp.cast()}
unsafe fn is_err<T>(p:*mut T)->bool {(p as usize)>=usize::MAX-4094}
// upstream: i915_gem_phys.c __set_phys_vaddr()
unsafe fn __set_phys_vaddr(sg:*mut Scatterlist,vaddr:*mut c_void){(*sg).page_link=((*sg).page_link&3)|vaddr as usize;}
// upstream: i915_gem_phys.c __get_phys_vaddr()
unsafe fn __get_phys_vaddr(sg:*mut Scatterlist)->*mut c_void {sg_page(sg).cast()}
// upstream: i915_gem_phys.c i915_gem_object_get_pages_phys()
unsafe fn i915_gem_object_get_pages_phys(obj:*mut DrmI915GemObject)->c_int {
    let mapping=(*file(obj)).f_mapping;let i915=to_i915((*obj).base.base.dev);let size=(*obj).base.base.size as usize;
    if size>u32::MAX as usize {return -E2BIG;}
    if GEM_WARN_ON!(i915_gem_object_needs_bit17_swizzle(obj)){return -EINVAL;}
    let mut dma=0;let vaddr=dma_alloc_coherent(drm_device_device((*obj).base.base.dev),size.next_power_of_two(),&mut dma,GFP_KERNEL);
    if vaddr.is_null(){return -ENOMEM;}
    let st=kmalloc_obj::<SgTable>(GFP_KERNEL);
    if !st.is_null() {
        if sg_alloc_table(st,1,GFP_KERNEL)==0 {
            let sg=(*st).sgl;(*sg).offset=0;(*sg).length=size as u32;__set_phys_vaddr(sg,vaddr);(*sg).dma_address=dma;(*sg).dma_length=size as u32;
            let mut dst=vaddr.cast::<u8>();let mut error=false;
            for i in 0..size/4096 {
                let page=shmem_read_mapping_page(mapping,i as u64);if is_err(page){error=true;break;}
                let src=kmap_local_page(page);core::ptr::copy_nonoverlapping(src.cast::<u8>(),dst,4096);kunmap_local(src);
                drm_clflush_virt_range(dst.cast(),4096);put_page(page);dst=dst.add(4096);
            }
            if !error {intel_gt_chipset_flush(to_gt(i915));(*obj).mem_flags&=!1;__i915_gem_object_set_pages(obj,st);return 0;}
        }
        kfree(st);
    }
    dma_free_coherent(drm_device_device((*obj).base.base.dev),size.next_power_of_two(),vaddr,dma);-ENOMEM
}
// upstream: i915_gem_phys.c i915_gem_object_put_pages_phys()
pub unsafe fn i915_gem_object_put_pages_phys(obj:*mut DrmI915GemObject,pages:*mut SgTable){
    let dma=(*(*pages).sgl).dma_address;let vaddr=__get_phys_vaddr((*pages).sgl);
    __i915_gem_object_release_shmem(obj,pages,false);
    if (*obj).mm.is_dirty() {
        let mapping=(*file(obj)).f_mapping;let mut src=vaddr.cast::<u8>();
        for i in 0..(*obj).base.base.size/4096 {
            let page=shmem_read_mapping_page(mapping,i as u64);if is_err(page){continue;}
            drm_clflush_virt_range(src.cast(),4096);
            let dst=kmap_local_page(page);core::ptr::copy_nonoverlapping(src,dst.cast(),4096);kunmap_local(dst);
            folio_mark_dirty(page_folio(page));if (*obj).mm.madv()==0 {mark_page_accessed(page);}put_page(page);src=src.add(4096);
        }
        (*obj).mm.set_dirty(false);
    }
    sg_free_table(pages);kfree(pages);
    dma_free_coherent(drm_device_device((*obj).base.base.dev),((*obj).base.base.size as usize).next_power_of_two(),vaddr,dma);
}
// upstream: i915_gem_phys.c i915_gem_object_pwrite_phys()
pub unsafe fn i915_gem_object_pwrite_phys(obj:*mut DrmI915GemObject,args:*const DrmI915GemPwrite)->c_int {
    let vaddr=__get_phys_vaddr((*(*obj).mm.pages).sgl).cast::<u8>().add((*args).offset as usize).cast::<c_void>();
    let user=(*args).data_ptr as *const c_void;let i915=to_i915((*obj).base.base.dev);
    let err=i915_gem_object_wait(obj,3,MAX_SCHEDULE_TIMEOUT as i64);if err!=0{return err;}
    i915_gem_object_frontbuffer_invalidate(obj,0);
    if copy_from_user(vaddr,user,(*args).size as usize)!=0{return -EFAULT;}
    drm_clflush_virt_range(vaddr,(*args).size);intel_gt_chipset_flush(to_gt(i915));i915_gem_object_frontbuffer_flush(obj,0);0
}
// upstream: i915_gem_phys.c i915_gem_object_pread_phys()
pub unsafe fn i915_gem_object_pread_phys(obj:*mut DrmI915GemObject,args:*const DrmI915GemPread)->c_int {
    let vaddr=__get_phys_vaddr((*(*obj).mm.pages).sgl).cast::<u8>().add((*args).offset as usize).cast::<c_void>();
    let user=(*args).data_ptr as *mut c_void;
    let err=i915_gem_object_wait(obj,1,MAX_SCHEDULE_TIMEOUT as i64);if err!=0{return err;}
    drm_clflush_virt_range(vaddr,(*args).size);if copy_to_user(user,vaddr,(*args).size as usize)!=0{return -EFAULT;}0
}
// upstream: i915_gem_phys.c i915_gem_object_shmem_to_phys()
unsafe fn i915_gem_object_shmem_to_phys(obj:*mut DrmI915GemObject)->c_int {
    let pages=__i915_gem_object_unset_pages(obj);let err=i915_gem_object_get_pages_phys(obj);
    if err!=0 {if !pages.is_null()&&!is_err(pages){__i915_gem_object_set_pages(obj,pages);}return err;}
    __i915_gem_object_pin_pages(obj);if !pages.is_null()&&!is_err(pages){i915_gem_object_put_pages_shmem(obj,pages);}i915_gem_object_release_memory_region(obj);0
}
// upstream: i915_gem_phys.c i915_gem_object_attach_phys()
pub unsafe fn i915_gem_object_attach_phys(obj:*mut DrmI915GemObject,align:c_int)->c_int {
    assert_object_held(obj);
    if align as u64>(*obj).base.base.size{return -EINVAL;}
    if !i915_gem_object_is_shmem(obj){return -EINVAL;}
    if !i915_gem_object_has_struct_page(obj){return 0;}
    let err=i915_gem_object_unbind(obj,1);if err!=0{return err;}
    if (*obj).mm.madv()!=0{return -EFAULT;}
    if i915_gem_object_has_tiling_quirk(obj){return -EFAULT;}
    if !(*obj).mm.mapping.is_null()||i915_gem_object_has_pinned_pages(obj){return -EBUSY;}
    if (*obj).mm.madv()!=0 {axlog::debug!("Attempting to obtain a purgeable object");return -EFAULT;}
    i915_gem_object_shmem_to_phys(obj)
}
