# DRM property UAPI source translation

`kernel/src/drm/property_uapi_full.rs` translates all 26 function definitions
from Linux v7.2.3 `drivers/gpu/drm/drm_property.c` (984 Rust lines). The source
has an explicit permissive Intel grant (Copyright (c) 2016 Intel Corporation);
the complete notice is retained in the source and
`kernel/LICENSES/LicenseRef-Intel-Drm-Property-MIT`.

Property/blob lifetime, enumeration, validation, and object attachment policy
are translated behind the `PropertyUapiIo` boundary. The source markers match
ctags function order exactly (26/26), and the module is compiled by the kernel
crate. It is a reference translation only: current `property.rs` remains the
live implementation, and the generic ioctl/property lifetimes have not been
replaced by this module.
