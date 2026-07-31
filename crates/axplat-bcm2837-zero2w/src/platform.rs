//! Platform identity.

use ax_plat::platform::PlatformInfoIf;

struct PlatformInfoIfImpl;

#[impl_plat_interface]
impl PlatformInfoIf for PlatformInfoIfImpl {
    fn platform_name() -> &'static str {
        "bcm2837-zero2w"
    }
}
