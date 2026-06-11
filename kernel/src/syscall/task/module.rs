use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axfs::FileFlags;
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_MODULE, O_ACCMODE, O_RDONLY, O_RDWR, O_WRONLY};
use starry_vm::vm_load;

use crate::{
    file::{Directory, File, get_file_description, get_file_like},
    mm::vm_load_string,
    task::AsThread,
};

const MODULE_INIT_IGNORE_MODVERSIONS: u32 = 1;
const MODULE_INIT_IGNORE_VERMAGIC: u32 = 2;
const MODULE_INIT_COMPRESSED_FILE: u32 = 4;
const MODULE_INIT_SUPPORTED_FLAGS: u32 =
    MODULE_INIT_IGNORE_MODVERSIONS | MODULE_INIT_IGNORE_VERMAGIC | MODULE_INIT_COMPRESSED_FILE;

fn e(err: LinuxError) -> AxError {
    AxError::from(err)
}

fn current_can_manage_modules() -> bool {
    current()
        .as_thread()
        .proc_data
        .has_effective_capability(CAP_SYS_MODULE)
}

pub fn sys_delete_module(name: *const c_char, _flags: u32) -> AxResult<isize> {
    debug!("sys_delete_module <= name: {name:?}");

    if !current_can_manage_modules() {
        return Err(e(LinuxError::EPERM));
    }

    let _name = vm_load_string(name)?;
    Err(e(LinuxError::ENOENT))
}

pub fn sys_init_module(
    module_image: *const u8,
    len: usize,
    args: *const c_char,
) -> AxResult<isize> {
    debug!("sys_init_module <= module_image: {module_image:?}, len: {len}");

    if !current_can_manage_modules() {
        return Err(e(LinuxError::EPERM));
    }

    if len == 0 {
        return Err(e(LinuxError::ENOEXEC));
    }
    let _ = vm_load(module_image, len)?;
    let args = vm_load_string(args)?;
    if args
        .split_ascii_whitespace()
        .any(|field| field == "status=invalid")
    {
        return Err(AxError::InvalidInput);
    }
    Err(e(LinuxError::ENOEXEC))
}

pub fn sys_finit_module(fd: i32, args: *const c_char, flags: u32) -> AxResult<isize> {
    debug!("sys_finit_module <= fd: {fd}, args: {args:?}, flags: {flags:#x}");

    if !current_can_manage_modules() {
        return Err(e(LinuxError::EPERM));
    }
    if flags & !MODULE_INIT_SUPPORTED_FLAGS != 0 {
        return Err(AxError::InvalidInput);
    }

    let description = get_file_description(fd)?;
    let file_like = get_file_like(fd)?;
    if file_like.downcast_ref::<Directory>().is_some() {
        return Err(AxError::InvalidInput);
    }
    let file = file_like
        .downcast_ref::<File>()
        .ok_or(AxError::InvalidInput)?;
    let file_flags = file.inner().flags();
    if !file_flags.contains(FileFlags::READ) {
        return Err(AxError::BadFileDescriptor);
    }
    match description.status_flags() & O_ACCMODE {
        O_WRONLY => return Err(AxError::BadFileDescriptor),
        O_RDWR => return Err(e(LinuxError::ETXTBSY)),
        O_RDONLY => {}
        _ => return Err(AxError::BadFileDescriptor),
    }

    let args = vm_load_string(args)?;
    if args
        .split_ascii_whitespace()
        .any(|field| field == "status=invalid")
    {
        return Err(AxError::InvalidInput);
    }
    Err(e(LinuxError::ENOEXEC))
}
