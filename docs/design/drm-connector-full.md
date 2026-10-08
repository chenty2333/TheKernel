# DRM connector helper translation

`kernel::drm::connector_uapi_full` translates all 87 ctags definitions in
Linux 7.2.3 `drivers/gpu/drm/drm_connector.c`, plus the source-defined
`drm_get_tv_mode_from_name()` that ctags did not identify (88 marked
functions total; 2,804 Rust lines). The MIT-style license is retained in the
source and `kernel/LICENSES/LicenseRef-Intel-Drm-Connector-MIT`.

The module carries connector status/property, EDID, display-mode/tile/TV,
privacy-screen, color-property, and atomic-state policy; DRM object/registry,
blob references, usercopy, mode locks and hardware probes are hooks. It is
declared and compiles in `tk-kernel`, but no `DrmConnectorIo` adapter currently
routes the active single-connector ioctl path through it. Multi-connector and
multi-CRTC enumeration remain unimplemented.
