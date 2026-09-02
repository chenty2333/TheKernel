#define _GNU_SOURCE
/*
 * Linux DRM UAPI oracle.  Build without libdrm:
 *   cc -std=c11 -Wall -Wextra -Werror -o drm-uapi-oracle drm-uapi-oracle.c
 *
 * Output is deliberately line-oriented and machine readable.  A missing DRM
 * device, or an operation unavailable on this particular virtual GPU, is a
 * SKIP rather than a failure: this probe records the Linux ABI surface.
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/types.h>
#include <unistd.h>

#include <drm/drm.h>
#include <drm/drm_mode.h>
#include <drm/virtgpu_drm.h>

#if !defined(__x86_64__)
#error "graphics oracle requires the x86_64 Linux ABI"
#endif

#define DRM_EVENT_FLIP_COMPLETE 0x02
#define ABI_STRUCT(type) printf("TK_DRM_ABI struct=%s size=%zu align=%zu\n", #type, sizeof(type), _Alignof(type))
#define ABI_FIELD(type, field) printf("TK_DRM_ABI field=%s.%s offset=%zu\n", #type, #field, offsetof(type, field))
#define ABI_IOCTL(name) printf("TK_DRM_ABI ioctl=%s value=0x%lx\n", #name, (unsigned long)(name))

static void abi_oracle(void) {
    ABI_STRUCT(struct drm_version); ABI_FIELD(struct drm_version, version_major); ABI_FIELD(struct drm_version, version_minor); ABI_FIELD(struct drm_version, version_patchlevel); ABI_FIELD(struct drm_version, name_len); ABI_FIELD(struct drm_version, name); ABI_FIELD(struct drm_version, date_len); ABI_FIELD(struct drm_version, date); ABI_FIELD(struct drm_version, desc_len); ABI_FIELD(struct drm_version, desc);
    ABI_STRUCT(struct drm_auth); ABI_FIELD(struct drm_auth, magic);
    ABI_STRUCT(struct drm_get_cap); ABI_FIELD(struct drm_get_cap, capability); ABI_FIELD(struct drm_get_cap, value);
    ABI_STRUCT(struct drm_set_client_cap); ABI_FIELD(struct drm_set_client_cap, capability); ABI_FIELD(struct drm_set_client_cap, value);
    ABI_STRUCT(struct drm_gem_close); ABI_FIELD(struct drm_gem_close, handle); ABI_FIELD(struct drm_gem_close, pad);
    ABI_STRUCT(struct drm_prime_handle); ABI_FIELD(struct drm_prime_handle, handle); ABI_FIELD(struct drm_prime_handle, flags); ABI_FIELD(struct drm_prime_handle, fd);
    ABI_STRUCT(struct drm_set_version); ABI_FIELD(struct drm_set_version, drm_di_major); ABI_FIELD(struct drm_set_version, drm_di_minor); ABI_FIELD(struct drm_set_version, drm_dd_major); ABI_FIELD(struct drm_set_version, drm_dd_minor);
    ABI_STRUCT(struct drm_mode_modeinfo); ABI_FIELD(struct drm_mode_modeinfo, clock); ABI_FIELD(struct drm_mode_modeinfo, hdisplay); ABI_FIELD(struct drm_mode_modeinfo, hsync_start); ABI_FIELD(struct drm_mode_modeinfo, hsync_end); ABI_FIELD(struct drm_mode_modeinfo, htotal); ABI_FIELD(struct drm_mode_modeinfo, hskew); ABI_FIELD(struct drm_mode_modeinfo, vdisplay); ABI_FIELD(struct drm_mode_modeinfo, vsync_start); ABI_FIELD(struct drm_mode_modeinfo, vsync_end); ABI_FIELD(struct drm_mode_modeinfo, vtotal); ABI_FIELD(struct drm_mode_modeinfo, vscan); ABI_FIELD(struct drm_mode_modeinfo, vrefresh); ABI_FIELD(struct drm_mode_modeinfo, flags); ABI_FIELD(struct drm_mode_modeinfo, type); ABI_FIELD(struct drm_mode_modeinfo, name);
    ABI_STRUCT(struct drm_mode_card_res); ABI_FIELD(struct drm_mode_card_res, fb_id_ptr); ABI_FIELD(struct drm_mode_card_res, crtc_id_ptr); ABI_FIELD(struct drm_mode_card_res, connector_id_ptr); ABI_FIELD(struct drm_mode_card_res, encoder_id_ptr); ABI_FIELD(struct drm_mode_card_res, count_fbs); ABI_FIELD(struct drm_mode_card_res, count_crtcs); ABI_FIELD(struct drm_mode_card_res, count_connectors); ABI_FIELD(struct drm_mode_card_res, count_encoders); ABI_FIELD(struct drm_mode_card_res, min_width); ABI_FIELD(struct drm_mode_card_res, max_width); ABI_FIELD(struct drm_mode_card_res, min_height); ABI_FIELD(struct drm_mode_card_res, max_height);
    ABI_STRUCT(struct drm_mode_crtc); ABI_FIELD(struct drm_mode_crtc, set_connectors_ptr); ABI_FIELD(struct drm_mode_crtc, count_connectors); ABI_FIELD(struct drm_mode_crtc, crtc_id); ABI_FIELD(struct drm_mode_crtc, fb_id); ABI_FIELD(struct drm_mode_crtc, x); ABI_FIELD(struct drm_mode_crtc, y); ABI_FIELD(struct drm_mode_crtc, gamma_size); ABI_FIELD(struct drm_mode_crtc, mode_valid); ABI_FIELD(struct drm_mode_crtc, mode);
    ABI_STRUCT(struct drm_mode_crtc_lut); ABI_FIELD(struct drm_mode_crtc_lut, crtc_id); ABI_FIELD(struct drm_mode_crtc_lut, red); ABI_FIELD(struct drm_mode_crtc_lut, green); ABI_FIELD(struct drm_mode_crtc_lut, blue);
    ABI_STRUCT(struct drm_mode_get_encoder); ABI_FIELD(struct drm_mode_get_encoder, encoder_id); ABI_FIELD(struct drm_mode_get_encoder, encoder_type); ABI_FIELD(struct drm_mode_get_encoder, crtc_id); ABI_FIELD(struct drm_mode_get_encoder, possible_crtcs); ABI_FIELD(struct drm_mode_get_encoder, possible_clones);
    ABI_STRUCT(struct drm_mode_get_connector); ABI_FIELD(struct drm_mode_get_connector, encoders_ptr); ABI_FIELD(struct drm_mode_get_connector, modes_ptr); ABI_FIELD(struct drm_mode_get_connector, props_ptr); ABI_FIELD(struct drm_mode_get_connector, prop_values_ptr); ABI_FIELD(struct drm_mode_get_connector, count_modes); ABI_FIELD(struct drm_mode_get_connector, count_props); ABI_FIELD(struct drm_mode_get_connector, count_encoders); ABI_FIELD(struct drm_mode_get_connector, encoder_id); ABI_FIELD(struct drm_mode_get_connector, connector_id); ABI_FIELD(struct drm_mode_get_connector, connector_type); ABI_FIELD(struct drm_mode_get_connector, connector_type_id); ABI_FIELD(struct drm_mode_get_connector, connection); ABI_FIELD(struct drm_mode_get_connector, mm_width); ABI_FIELD(struct drm_mode_get_connector, mm_height); ABI_FIELD(struct drm_mode_get_connector, subpixel); ABI_FIELD(struct drm_mode_get_connector, pad);
    ABI_STRUCT(struct drm_mode_set_plane); ABI_FIELD(struct drm_mode_set_plane, plane_id); ABI_FIELD(struct drm_mode_set_plane, crtc_id); ABI_FIELD(struct drm_mode_set_plane, fb_id); ABI_FIELD(struct drm_mode_set_plane, flags); ABI_FIELD(struct drm_mode_set_plane, crtc_x); ABI_FIELD(struct drm_mode_set_plane, crtc_y); ABI_FIELD(struct drm_mode_set_plane, crtc_w); ABI_FIELD(struct drm_mode_set_plane, crtc_h); ABI_FIELD(struct drm_mode_set_plane, src_x); ABI_FIELD(struct drm_mode_set_plane, src_y); ABI_FIELD(struct drm_mode_set_plane, src_h); ABI_FIELD(struct drm_mode_set_plane, src_w);
    ABI_STRUCT(struct drm_mode_get_plane); ABI_FIELD(struct drm_mode_get_plane, plane_id); ABI_FIELD(struct drm_mode_get_plane, crtc_id); ABI_FIELD(struct drm_mode_get_plane, fb_id); ABI_FIELD(struct drm_mode_get_plane, possible_crtcs); ABI_FIELD(struct drm_mode_get_plane, gamma_size); ABI_FIELD(struct drm_mode_get_plane, count_format_types); ABI_FIELD(struct drm_mode_get_plane, format_type_ptr);
    ABI_STRUCT(struct drm_mode_get_plane_res); ABI_FIELD(struct drm_mode_get_plane_res, plane_id_ptr); ABI_FIELD(struct drm_mode_get_plane_res, count_planes);
    ABI_STRUCT(struct drm_mode_property_enum); ABI_FIELD(struct drm_mode_property_enum, value); ABI_FIELD(struct drm_mode_property_enum, name);
    ABI_STRUCT(struct drm_mode_get_property); ABI_FIELD(struct drm_mode_get_property, values_ptr); ABI_FIELD(struct drm_mode_get_property, enum_blob_ptr); ABI_FIELD(struct drm_mode_get_property, prop_id); ABI_FIELD(struct drm_mode_get_property, flags); ABI_FIELD(struct drm_mode_get_property, name); ABI_FIELD(struct drm_mode_get_property, count_values); ABI_FIELD(struct drm_mode_get_property, count_enum_blobs);
    ABI_STRUCT(struct drm_mode_connector_set_property); ABI_FIELD(struct drm_mode_connector_set_property, value); ABI_FIELD(struct drm_mode_connector_set_property, prop_id); ABI_FIELD(struct drm_mode_connector_set_property, connector_id);
    ABI_STRUCT(struct drm_mode_obj_get_properties); ABI_FIELD(struct drm_mode_obj_get_properties, props_ptr); ABI_FIELD(struct drm_mode_obj_get_properties, prop_values_ptr); ABI_FIELD(struct drm_mode_obj_get_properties, count_props); ABI_FIELD(struct drm_mode_obj_get_properties, obj_id); ABI_FIELD(struct drm_mode_obj_get_properties, obj_type);
    ABI_STRUCT(struct drm_mode_obj_set_property); ABI_FIELD(struct drm_mode_obj_set_property, value); ABI_FIELD(struct drm_mode_obj_set_property, prop_id); ABI_FIELD(struct drm_mode_obj_set_property, obj_id); ABI_FIELD(struct drm_mode_obj_set_property, obj_type);
    ABI_STRUCT(struct drm_mode_get_blob); ABI_FIELD(struct drm_mode_get_blob, blob_id); ABI_FIELD(struct drm_mode_get_blob, data);
    ABI_STRUCT(struct drm_mode_create_blob); ABI_FIELD(struct drm_mode_create_blob, data); ABI_FIELD(struct drm_mode_create_blob, length); ABI_FIELD(struct drm_mode_create_blob, blob_id);
    ABI_STRUCT(struct drm_mode_destroy_blob); ABI_FIELD(struct drm_mode_destroy_blob, blob_id);
    ABI_STRUCT(struct drm_mode_fb_cmd); ABI_FIELD(struct drm_mode_fb_cmd, fb_id); ABI_FIELD(struct drm_mode_fb_cmd, width); ABI_FIELD(struct drm_mode_fb_cmd, height); ABI_FIELD(struct drm_mode_fb_cmd, pitch); ABI_FIELD(struct drm_mode_fb_cmd, bpp); ABI_FIELD(struct drm_mode_fb_cmd, depth); ABI_FIELD(struct drm_mode_fb_cmd, handle);
    ABI_STRUCT(struct drm_mode_fb_cmd2); ABI_FIELD(struct drm_mode_fb_cmd2, fb_id); ABI_FIELD(struct drm_mode_fb_cmd2, handles); ABI_FIELD(struct drm_mode_fb_cmd2, pitches); ABI_FIELD(struct drm_mode_fb_cmd2, offsets); ABI_FIELD(struct drm_mode_fb_cmd2, modifier);
    ABI_STRUCT(struct drm_mode_fb_dirty_cmd); ABI_FIELD(struct drm_mode_fb_dirty_cmd, fb_id); ABI_FIELD(struct drm_mode_fb_dirty_cmd, flags); ABI_FIELD(struct drm_mode_fb_dirty_cmd, color); ABI_FIELD(struct drm_mode_fb_dirty_cmd, num_clips); ABI_FIELD(struct drm_mode_fb_dirty_cmd, clips_ptr);
    ABI_STRUCT(struct drm_mode_cursor); ABI_FIELD(struct drm_mode_cursor, flags); ABI_FIELD(struct drm_mode_cursor, crtc_id); ABI_FIELD(struct drm_mode_cursor, x); ABI_FIELD(struct drm_mode_cursor, y); ABI_FIELD(struct drm_mode_cursor, width); ABI_FIELD(struct drm_mode_cursor, height); ABI_FIELD(struct drm_mode_cursor, handle);
    ABI_STRUCT(struct drm_mode_cursor2); ABI_FIELD(struct drm_mode_cursor2, flags); ABI_FIELD(struct drm_mode_cursor2, crtc_id); ABI_FIELD(struct drm_mode_cursor2, x); ABI_FIELD(struct drm_mode_cursor2, y); ABI_FIELD(struct drm_mode_cursor2, width); ABI_FIELD(struct drm_mode_cursor2, height); ABI_FIELD(struct drm_mode_cursor2, handle); ABI_FIELD(struct drm_mode_cursor2, hot_x); ABI_FIELD(struct drm_mode_cursor2, hot_y);
    ABI_STRUCT(struct drm_mode_crtc_page_flip); ABI_FIELD(struct drm_mode_crtc_page_flip, crtc_id); ABI_FIELD(struct drm_mode_crtc_page_flip, fb_id); ABI_FIELD(struct drm_mode_crtc_page_flip, flags); ABI_FIELD(struct drm_mode_crtc_page_flip, reserved); ABI_FIELD(struct drm_mode_crtc_page_flip, user_data);
    ABI_STRUCT(struct drm_mode_create_dumb); ABI_FIELD(struct drm_mode_create_dumb, height); ABI_FIELD(struct drm_mode_create_dumb, width); ABI_FIELD(struct drm_mode_create_dumb, bpp); ABI_FIELD(struct drm_mode_create_dumb, flags); ABI_FIELD(struct drm_mode_create_dumb, handle); ABI_FIELD(struct drm_mode_create_dumb, pitch); ABI_FIELD(struct drm_mode_create_dumb, size);
    ABI_STRUCT(struct drm_mode_map_dumb); ABI_FIELD(struct drm_mode_map_dumb, handle); ABI_FIELD(struct drm_mode_map_dumb, offset);
    ABI_STRUCT(struct drm_mode_destroy_dumb); ABI_FIELD(struct drm_mode_destroy_dumb, handle);
    ABI_STRUCT(struct drm_mode_atomic); ABI_FIELD(struct drm_mode_atomic, flags); ABI_FIELD(struct drm_mode_atomic, count_objs); ABI_FIELD(struct drm_mode_atomic, objs_ptr); ABI_FIELD(struct drm_mode_atomic, count_props_ptr); ABI_FIELD(struct drm_mode_atomic, props_ptr); ABI_FIELD(struct drm_mode_atomic, prop_values_ptr); ABI_FIELD(struct drm_mode_atomic, reserved); ABI_FIELD(struct drm_mode_atomic, user_data);
    ABI_STRUCT(struct drm_wait_vblank_request); ABI_FIELD(struct drm_wait_vblank_request, type); ABI_FIELD(struct drm_wait_vblank_request, sequence); ABI_FIELD(struct drm_wait_vblank_request, signal);
    ABI_STRUCT(struct drm_wait_vblank_reply); ABI_FIELD(struct drm_wait_vblank_reply, type); ABI_FIELD(struct drm_wait_vblank_reply, sequence); ABI_FIELD(struct drm_wait_vblank_reply, tval_sec); ABI_FIELD(struct drm_wait_vblank_reply, tval_usec); ABI_STRUCT(union drm_wait_vblank);
    ABI_STRUCT(struct drm_syncobj_create); ABI_FIELD(struct drm_syncobj_create, handle); ABI_FIELD(struct drm_syncobj_create, flags);
    ABI_STRUCT(struct drm_syncobj_destroy); ABI_FIELD(struct drm_syncobj_destroy, handle); ABI_FIELD(struct drm_syncobj_destroy, pad);
    ABI_STRUCT(struct drm_syncobj_handle); ABI_FIELD(struct drm_syncobj_handle, handle); ABI_FIELD(struct drm_syncobj_handle, flags); ABI_FIELD(struct drm_syncobj_handle, fd); ABI_FIELD(struct drm_syncobj_handle, pad);
    ABI_STRUCT(struct drm_syncobj_transfer); ABI_FIELD(struct drm_syncobj_transfer, src_handle); ABI_FIELD(struct drm_syncobj_transfer, dst_handle); ABI_FIELD(struct drm_syncobj_transfer, src_point); ABI_FIELD(struct drm_syncobj_transfer, dst_point); ABI_FIELD(struct drm_syncobj_transfer, flags); ABI_FIELD(struct drm_syncobj_transfer, pad);
    ABI_STRUCT(struct drm_syncobj_wait); ABI_FIELD(struct drm_syncobj_wait, handles); ABI_FIELD(struct drm_syncobj_wait, timeout_nsec); ABI_FIELD(struct drm_syncobj_wait, count_handles); ABI_FIELD(struct drm_syncobj_wait, flags); ABI_FIELD(struct drm_syncobj_wait, first_signaled); ABI_FIELD(struct drm_syncobj_wait, pad); ABI_FIELD(struct drm_syncobj_wait, deadline_nsec);
    ABI_STRUCT(struct drm_syncobj_timeline_wait); ABI_FIELD(struct drm_syncobj_timeline_wait, handles); ABI_FIELD(struct drm_syncobj_timeline_wait, points); ABI_FIELD(struct drm_syncobj_timeline_wait, timeout_nsec); ABI_FIELD(struct drm_syncobj_timeline_wait, count_handles); ABI_FIELD(struct drm_syncobj_timeline_wait, flags); ABI_FIELD(struct drm_syncobj_timeline_wait, first_signaled); ABI_FIELD(struct drm_syncobj_timeline_wait, pad); ABI_FIELD(struct drm_syncobj_timeline_wait, deadline_nsec);
    ABI_STRUCT(struct drm_syncobj_eventfd); ABI_FIELD(struct drm_syncobj_eventfd, handle); ABI_FIELD(struct drm_syncobj_eventfd, flags); ABI_FIELD(struct drm_syncobj_eventfd, point); ABI_FIELD(struct drm_syncobj_eventfd, fd); ABI_FIELD(struct drm_syncobj_eventfd, pad);
    ABI_STRUCT(struct drm_syncobj_array); ABI_FIELD(struct drm_syncobj_array, handles); ABI_FIELD(struct drm_syncobj_array, count_handles);
    ABI_STRUCT(struct drm_syncobj_timeline_array); ABI_FIELD(struct drm_syncobj_timeline_array, handles); ABI_FIELD(struct drm_syncobj_timeline_array, points); ABI_FIELD(struct drm_syncobj_timeline_array, count_handles); ABI_FIELD(struct drm_syncobj_timeline_array, flags);
    ABI_IOCTL(DRM_IOCTL_VERSION); ABI_IOCTL(DRM_IOCTL_GET_MAGIC); ABI_IOCTL(DRM_IOCTL_SET_VERSION); ABI_IOCTL(DRM_IOCTL_GEM_CLOSE); ABI_IOCTL(DRM_IOCTL_GET_CAP); ABI_IOCTL(DRM_IOCTL_SET_CLIENT_CAP); ABI_IOCTL(DRM_IOCTL_AUTH_MAGIC); ABI_IOCTL(DRM_IOCTL_SET_MASTER); ABI_IOCTL(DRM_IOCTL_DROP_MASTER); ABI_IOCTL(DRM_IOCTL_PRIME_HANDLE_TO_FD); ABI_IOCTL(DRM_IOCTL_PRIME_FD_TO_HANDLE); ABI_IOCTL(DRM_IOCTL_WAIT_VBLANK);
    ABI_IOCTL(DRM_IOCTL_MODE_GETRESOURCES); ABI_IOCTL(DRM_IOCTL_MODE_GETCRTC); ABI_IOCTL(DRM_IOCTL_MODE_SETCRTC); ABI_IOCTL(DRM_IOCTL_MODE_CURSOR); ABI_IOCTL(DRM_IOCTL_MODE_GETGAMMA); ABI_IOCTL(DRM_IOCTL_MODE_SETGAMMA); ABI_IOCTL(DRM_IOCTL_MODE_GETENCODER); ABI_IOCTL(DRM_IOCTL_MODE_GETCONNECTOR); ABI_IOCTL(DRM_IOCTL_MODE_GETPROPERTY); ABI_IOCTL(DRM_IOCTL_MODE_SETPROPERTY); ABI_IOCTL(DRM_IOCTL_MODE_GETPROPBLOB); ABI_IOCTL(DRM_IOCTL_MODE_GETFB); ABI_IOCTL(DRM_IOCTL_MODE_ADDFB); ABI_IOCTL(DRM_IOCTL_MODE_RMFB); ABI_IOCTL(DRM_IOCTL_MODE_PAGE_FLIP); ABI_IOCTL(DRM_IOCTL_MODE_DIRTYFB); ABI_IOCTL(DRM_IOCTL_MODE_CREATE_DUMB); ABI_IOCTL(DRM_IOCTL_MODE_MAP_DUMB); ABI_IOCTL(DRM_IOCTL_MODE_DESTROY_DUMB); ABI_IOCTL(DRM_IOCTL_MODE_GETPLANERESOURCES); ABI_IOCTL(DRM_IOCTL_MODE_GETPLANE); ABI_IOCTL(DRM_IOCTL_MODE_SETPLANE); ABI_IOCTL(DRM_IOCTL_MODE_ADDFB2); ABI_IOCTL(DRM_IOCTL_MODE_OBJ_GETPROPERTIES); ABI_IOCTL(DRM_IOCTL_MODE_OBJ_SETPROPERTY); ABI_IOCTL(DRM_IOCTL_MODE_CURSOR2); ABI_IOCTL(DRM_IOCTL_MODE_ATOMIC); ABI_IOCTL(DRM_IOCTL_MODE_CREATEPROPBLOB); ABI_IOCTL(DRM_IOCTL_MODE_DESTROYPROPBLOB); ABI_IOCTL(DRM_IOCTL_MODE_GETFB2);
    ABI_IOCTL(DRM_IOCTL_SYNCOBJ_CREATE); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_DESTROY); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_WAIT); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_RESET); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_SIGNAL); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_QUERY); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_TRANSFER); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL); ABI_IOCTL(DRM_IOCTL_SYNCOBJ_EVENTFD);
    ABI_STRUCT(struct drm_virtgpu_map); ABI_FIELD(struct drm_virtgpu_map, offset); ABI_FIELD(struct drm_virtgpu_map, handle); ABI_FIELD(struct drm_virtgpu_map, pad);
    ABI_STRUCT(struct drm_virtgpu_execbuffer); ABI_FIELD(struct drm_virtgpu_execbuffer, flags); ABI_FIELD(struct drm_virtgpu_execbuffer, command); ABI_FIELD(struct drm_virtgpu_execbuffer, bo_handles); ABI_FIELD(struct drm_virtgpu_execbuffer, num_bo_handles); ABI_FIELD(struct drm_virtgpu_execbuffer, fence_fd); ABI_FIELD(struct drm_virtgpu_execbuffer, ring_idx); ABI_FIELD(struct drm_virtgpu_execbuffer, syncobj_stride); ABI_FIELD(struct drm_virtgpu_execbuffer, num_in_syncobjs); ABI_FIELD(struct drm_virtgpu_execbuffer, num_out_syncobjs); ABI_FIELD(struct drm_virtgpu_execbuffer, in_syncobjs); ABI_FIELD(struct drm_virtgpu_execbuffer, out_syncobjs);
    ABI_STRUCT(struct drm_virtgpu_getparam); ABI_FIELD(struct drm_virtgpu_getparam, param); ABI_FIELD(struct drm_virtgpu_getparam, value);
    ABI_STRUCT(struct drm_virtgpu_resource_create); ABI_FIELD(struct drm_virtgpu_resource_create, target); ABI_FIELD(struct drm_virtgpu_resource_create, bo_handle); ABI_FIELD(struct drm_virtgpu_resource_create, stride);
    ABI_STRUCT(struct drm_virtgpu_resource_info); ABI_FIELD(struct drm_virtgpu_resource_info, bo_handle); ABI_FIELD(struct drm_virtgpu_resource_info, blob_mem);
    ABI_STRUCT(struct drm_virtgpu_3d_box); ABI_FIELD(struct drm_virtgpu_3d_box, x); ABI_FIELD(struct drm_virtgpu_3d_box, d);
    ABI_STRUCT(struct drm_virtgpu_3d_transfer_to_host); ABI_FIELD(struct drm_virtgpu_3d_transfer_to_host, bo_handle); ABI_FIELD(struct drm_virtgpu_3d_transfer_to_host, box); ABI_FIELD(struct drm_virtgpu_3d_transfer_to_host, layer_stride);
    ABI_STRUCT(struct drm_virtgpu_3d_transfer_from_host); ABI_FIELD(struct drm_virtgpu_3d_transfer_from_host, bo_handle); ABI_FIELD(struct drm_virtgpu_3d_transfer_from_host, box); ABI_FIELD(struct drm_virtgpu_3d_transfer_from_host, layer_stride);
    ABI_STRUCT(struct drm_virtgpu_3d_wait); ABI_FIELD(struct drm_virtgpu_3d_wait, handle); ABI_FIELD(struct drm_virtgpu_3d_wait, flags);
    ABI_STRUCT(struct drm_virtgpu_get_caps); ABI_FIELD(struct drm_virtgpu_get_caps, cap_set_id); ABI_FIELD(struct drm_virtgpu_get_caps, addr); ABI_FIELD(struct drm_virtgpu_get_caps, size); ABI_FIELD(struct drm_virtgpu_get_caps, pad);
    ABI_IOCTL(DRM_IOCTL_VIRTGPU_MAP); ABI_IOCTL(DRM_IOCTL_VIRTGPU_EXECBUFFER); ABI_IOCTL(DRM_IOCTL_VIRTGPU_GETPARAM); ABI_IOCTL(DRM_IOCTL_VIRTGPU_RESOURCE_CREATE); ABI_IOCTL(DRM_IOCTL_VIRTGPU_RESOURCE_INFO); ABI_IOCTL(DRM_IOCTL_VIRTGPU_TRANSFER_FROM_HOST); ABI_IOCTL(DRM_IOCTL_VIRTGPU_TRANSFER_TO_HOST); ABI_IOCTL(DRM_IOCTL_VIRTGPU_WAIT); ABI_IOCTL(DRM_IOCTL_VIRTGPU_GET_CAPS);
}

static int failures;

static void result(const char *kind, const char *state, int err) {
    if (strcmp(state, "FAIL") == 0) failures++;
    printf("TK_GRAPHICS kind=%s state=%s errno=%d\n", kind, state, err);
}

static int open_drm(const char *prefix, int first, int last, char *path, size_t size) {
    for (int index = first; index <= last; ++index) {
        int written = snprintf(path, size, "/dev/dri/%s%d", prefix, index);
        if (written < 0 || (size_t)written >= size) return -1;
        int fd = open(path, O_RDWR | O_CLOEXEC);
        if (fd >= 0) return fd;
    }
    return -1;
}

static void resources(int fd) {
    struct drm_mode_card_res res;
    memset(&res, 0, sizeof(res));
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
        result("drm.resources.phase1", "SKIP", errno); return;
    }
    __u32 crtcs[32], fbs[32], connectors[32], encoders[32];
    unsigned int nc = res.count_crtcs > 32 ? 32 : res.count_crtcs;
    unsigned int nf = res.count_fbs > 32 ? 32 : res.count_fbs;
    unsigned int nn = res.count_connectors > 32 ? 32 : res.count_connectors;
    unsigned int ne = res.count_encoders > 32 ? 32 : res.count_encoders;
    memset(crtcs, 0, sizeof(crtcs)); memset(fbs, 0, sizeof(fbs));
    memset(connectors, 0, sizeof(connectors)); memset(encoders, 0, sizeof(encoders));
    res.crtc_id_ptr = (uintptr_t)crtcs; res.fb_id_ptr = (uintptr_t)fbs;
    res.connector_id_ptr = (uintptr_t)connectors; res.encoder_id_ptr = (uintptr_t)encoders;
    res.count_crtcs = nc; res.count_fbs = nf; res.count_connectors = nn; res.count_encoders = ne;
    if (ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, &res) != 0) {
        result("drm.resources.phase2", "SKIP", errno); return;
    }
    printf("TK_GRAPHICS kind=drm.resources state=OK fbs=%u crtcs=%u connectors=%u encoders=%u\n",
           res.count_fbs, res.count_crtcs, res.count_connectors, res.count_encoders);
    if (res.count_crtcs && res.count_fbs) {
        struct drm_mode_crtc_page_flip flip = { .crtc_id = crtcs[0], .fb_id = fbs[0],
                                                 .flags = DRM_MODE_PAGE_FLIP_EVENT };
        if (ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP, &flip) == 0) {
            struct drm_event event;
            ssize_t n = read(fd, &event, sizeof(event));
            printf("TK_GRAPHICS kind=drm.page_flip_event state=OK read=%zd type=%u length=%u\n",
                   n, event.type, event.length);
        } else result("drm.page_flip_event", "SKIP", errno);
    } else result("drm.page_flip_event", "SKIP", ENODEV);
}

static void dumb_lifetime(int fd) {
    struct drm_mode_create_dumb dumb = { .width = 64, .height = 64, .bpp = 32 };
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) != 0) {
        result("drm.dumb_create", "SKIP", errno); return;
    }
    struct drm_mode_map_dumb map = { .handle = dumb.handle };
    if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) != 0) {
        result("drm.dumb_map", "FAIL", errno); return;
    }
    void *memory = mmap(NULL, dumb.size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, (off_t)map.offset);
    if (memory == MAP_FAILED) { result("drm.gem_mmap", "FAIL", errno); return; }
    ((volatile unsigned char *)memory)[0] = 0x5a;
    if (munmap(memory, dumb.size) != 0) { result("drm.gem_munmap", "FAIL", errno); return; }
    struct drm_mode_destroy_dumb destroy = { .handle = dumb.handle };
    if (ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy) != 0) { result("drm.dumb_destroy", "FAIL", errno); return; }
    printf("TK_GRAPHICS kind=drm.dumb_lifetime state=OK size=%" PRIu64 " pitch=%u\n",
           (uint64_t)dumb.size, dumb.pitch);
    memset(&dumb, 0, sizeof(dumb));
    dumb.width = 64; dumb.height = 64; dumb.bpp = 32;
    if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) != 0) { result("drm.gem_close_create", "FAIL", errno); return; }
    struct drm_gem_close close_handle = { .handle = dumb.handle };
    if (ioctl(fd, DRM_IOCTL_GEM_CLOSE, &close_handle) != 0) { result("drm.gem_close", "FAIL", errno); return; }
    memset(&map, 0, sizeof(map)); map.handle = dumb.handle;
    if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) == 0) {
        result("drm.gem_close_lifetime", "FAIL", 0); return;
    }
    printf("TK_GRAPHICS kind=drm.gem_close_lifetime state=OK\n");
}

int main(void) {
    char path[64];
    abi_oracle();
    printf("TK_GRAPHICS kind=drm.uapi state=OK card_res=%zu create_dumb=%zu map_dumb=%zu gem_close=%zu page_flip=%zu event=%zu flip_event=%zu getresources=0x%lx create=0x%lx map=0x%lx destroy=0x%lx gem_close_ioctl=0x%lx page_flip_ioctl=0x%lx\n",
           sizeof(struct drm_mode_card_res), sizeof(struct drm_mode_create_dumb), sizeof(struct drm_mode_map_dumb), sizeof(struct drm_gem_close), sizeof(struct drm_mode_crtc_page_flip), sizeof(struct drm_event), sizeof(struct drm_event_vblank),
           (unsigned long)DRM_IOCTL_MODE_GETRESOURCES, (unsigned long)DRM_IOCTL_MODE_CREATE_DUMB, (unsigned long)DRM_IOCTL_MODE_MAP_DUMB, (unsigned long)DRM_IOCTL_MODE_DESTROY_DUMB, (unsigned long)DRM_IOCTL_GEM_CLOSE, (unsigned long)DRM_IOCTL_MODE_PAGE_FLIP);
    int card = open_drm("card", 0, 15, path, sizeof(path));
    if (card < 0) result("drm.card", "SKIP", errno); else { printf("TK_GRAPHICS kind=drm.card state=OK node=card\n"); resources(card); dumb_lifetime(card); close(card); }
    int render = open_drm("renderD", 128, 143, path, sizeof(path));
    if (render < 0) result("drm.render", "SKIP", errno); else { printf("TK_GRAPHICS kind=drm.render state=OK node=render\n"); dumb_lifetime(render); close(render); }
    return failures == 0 ? 0 : 1;
}
