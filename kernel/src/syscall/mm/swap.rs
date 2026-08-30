use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs_ng_vfs::{Location, path::Path};
use axtask::current;
use linux_raw_sys::general::{AT_FDCWD, CAP_SYS_ADMIN, O_RDWR};
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, vm_load_until_nul};

use crate::{
    file::{ResolveAtResult, permission::{VfsSecurityContext, check_open_permissions_with_security}, resolve_at_with_security},
    mm::{activate, deactivate, map_usercopy_error},
    syscall::validate_pathname,
    task::AsThread,
};

fn admin() -> AxResult<()> {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_ADMIN)
        .then_some(())
        .ok_or_else(|| LinuxError::EPERM.into())
}
fn resolve<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    ptr: *const c_char,
) -> AxResult<Location> {
    let bytes = vm_load_until_nul(memory, ptr.cast()).map_err(map_usercopy_error)?;
    if bytes.is_empty() {
        return Err(LinuxError::ENOENT.into());
    }
    let path = core::str::from_utf8(&bytes).map_err(|_| AxError::IllegalBytes)?;
    let path = Path::new(path);
    validate_pathname(path)?;
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    match resolve_at_with_security(AT_FDCWD, Some(path.as_str()), 0, &security)? {
        ResolveAtResult::File(location) => Ok(location),
        ResolveAtResult::Other(_) => Err(AxError::InvalidInput),
    }
}
pub fn sys_swapon<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    specialfile: *const c_char,
    swap_flags: i32,
) -> AxResult<isize> {
    admin()?;
    let location = resolve(memory, specialfile)?;
    let security = VfsSecurityContext::new(current().as_thread().current_cred());
    check_open_permissions_with_security(
        &location,
        O_RDWR,
        security.actor(),
        security.credentials(),
        security.filesystem_owner_user_ns(),
    )?;
    activate(location, swap_flags)?;
    Ok(0)
}
pub fn sys_swapoff<M: UserMemory + ?Sized>(
    memory: &mut UserMemoryContext<'_, M>,
    specialfile: *const c_char,
) -> AxResult<isize> {
    admin()?;
    deactivate(&resolve(memory, specialfile)?)?;
    Ok(0)
}
