//! Native x86-64 ET_REL module admission.
//!
//! A native module supplies a defined `thekernel_module_init` symbol (and
//! optionally `thekernel_module_exit`) in an executable section. It may refer
//! only to symbols in that section until exported Rust symbols have a stable C
//! ABI. The loader nevertheless performs real ET_REL relocation and invokes
//! the published RX entry point.

use alloc::{string::String, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::CAP_SYS_MODULE;
use spin::Lazy;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded};

use crate::{
    jit_memory::{self, ExecutableCode, MemoryError},
    mm::map_usercopy_error,
    task::AsThread,
};

const MAX_MODULE_BYTES: usize = 16 * 1024 * 1024;
const MODULE_NAME_MAX: usize = 56;
const ELF64_EHDR: usize = 64;
const ELF64_SHDR: usize = 64;
const ELF64_SYM: usize = 24;
const ELF64_RELA: usize = 24;
const ET_REL: u16 = 1;
const EM_X86_64: u16 = 62;
const SHT_PROGBITS: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_RELA: u32 = 4;
const SHF_EXECINSTR: u64 = 4;
const SHN_UNDEF: u16 = 0;
const STT_FUNC: u8 = 2;

#[derive(Clone, Copy)]
struct Section { typ: u32, flags: u64, offset: usize, size: usize, link: u32, info: u32, entsize: usize }
#[derive(Clone, Copy)]
struct Symbol { name: u32, info: u8, section: u16, value: usize }
struct PreparedModule { name: String, code: ExecutableCode, init: usize, exit: Option<usize> }
static MODULES: Lazy<spin::Mutex<Vec<PreparedModule>>> = Lazy::new(|| spin::Mutex::new(Vec::new()));

fn le16(b: &[u8], o: usize) -> AxResult<u16> { b.get(o..o + 2).and_then(|v| v.try_into().ok()).map(u16::from_le_bytes).ok_or(AxError::InvalidExecutable) }
fn le32(b: &[u8], o: usize) -> AxResult<u32> { b.get(o..o + 4).and_then(|v| v.try_into().ok()).map(u32::from_le_bytes).ok_or(AxError::InvalidExecutable) }
fn le64(b: &[u8], o: usize) -> AxResult<u64> { b.get(o..o + 8).and_then(|v| v.try_into().ok()).map(u64::from_le_bytes).ok_or(AxError::InvalidExecutable) }
fn range(b: &[u8], o: usize, n: usize) -> AxResult<&[u8]> { b.get(o..o.checked_add(n).ok_or(AxError::InvalidExecutable)?).ok_or(AxError::InvalidExecutable) }
fn cstr(b: &[u8], o: usize) -> AxResult<&[u8]> { let tail=b.get(o..).ok_or(AxError::InvalidExecutable)?; Ok(&tail[..tail.iter().position(|x|*x==0).ok_or(AxError::InvalidExecutable)?]) }
fn has_module_capability() -> bool { current().as_thread().has_effective_capability(CAP_SYS_MODULE) }
fn memory_error(e: MemoryError) -> AxError { match e { MemoryError::Unavailable(e)|MemoryError::Quarantined(e)|MemoryError::Retained(e)=>e } }

fn sections(b: &[u8]) -> AxResult<Vec<Section>> {
    if b.len()<ELF64_EHDR || &b[..4]!=b"\x7fELF" || b[4]!=2 || b[5]!=1 || b[6]!=1 || le16(b,16)?!=ET_REL || le16(b,18)?!=EM_X86_64 { return Err(AxError::InvalidExecutable); }
    let off=usize::try_from(le64(b,40)?).map_err(|_|AxError::InvalidExecutable)?; let ent=usize::from(le16(b,58)?); let count=usize::from(le16(b,60)?);
    if ent!=ELF64_SHDR || count==0 { return Err(AxError::InvalidExecutable); } range(b,off,ent.checked_mul(count).ok_or(AxError::InvalidExecutable)?)?;
    (0..count).map(|i| { let p=off+i*ELF64_SHDR; Ok(Section { typ:le32(b,p+4)?, flags:le64(b,p+8)?, offset:usize::try_from(le64(b,p+24)?).map_err(|_|AxError::InvalidExecutable)?, size:usize::try_from(le64(b,p+32)?).map_err(|_|AxError::InvalidExecutable)?, link:le32(b,p+40)?, info:le32(b,p+44)?, entsize:usize::try_from(le64(b,p+56)?).map_err(|_|AxError::InvalidExecutable)? }) }).collect()
}
fn symbol(b:&[u8], tab:Section, i:usize)->AxResult<Symbol>{ if tab.entsize!=ELF64_SYM{return Err(AxError::InvalidExecutable)} let p=tab.offset.checked_add(i.checked_mul(ELF64_SYM).ok_or(AxError::InvalidExecutable)?).ok_or(AxError::InvalidExecutable)?; range(b,p,ELF64_SYM)?; Ok(Symbol{name:le32(b,p)?,info:b[p+4],section:le16(b,p+6)?,value:usize::try_from(le64(b,p+8)?).map_err(|_|AxError::InvalidExecutable)?}) }
fn name<'a>(b:&'a[u8],strs:Section,s:Symbol)->AxResult<&'a[u8]>{cstr(range(b,strs.offset,strs.size)?,s.name as usize)}

fn relocate(code:&mut[u8],base:usize,b:&[u8],ss:&[Section],code_index:usize)->AxResult<()> {
    for r in ss.iter().filter(|s|s.typ==SHT_RELA && s.info as usize==code_index) { let tab=*ss.get(r.link as usize).ok_or(AxError::InvalidExecutable)?; if tab.typ!=SHT_SYMTAB || r.entsize!=ELF64_RELA || r.size%ELF64_RELA!=0{return Err(AxError::InvalidExecutable)}; for n in 0..r.size/ELF64_RELA { let p=r.offset+n*ELF64_RELA; range(b,p,ELF64_RELA)?; let o=usize::try_from(le64(b,p)?).map_err(|_|AxError::InvalidExecutable)?; let info=le64(b,p+8)?; let sym=symbol(b,tab,(info>>32)as usize)?; if sym.section==SHN_UNDEF || sym.section as usize!=code_index{return Err(LinuxError::ENOEXEC.into())}; let s=base.checked_add(sym.value).ok_or(AxError::InvalidExecutable)? as i128; let a=le64(b,p+16)? as i64 as i128; let paddr=base.checked_add(o).ok_or(AxError::InvalidExecutable)? as i128; match info as u32 { 1=>{range(code,o,8)?;code[o..o+8].copy_from_slice(&((s+a)as u64).to_le_bytes())}, 2|4=>{range(code,o,4)?;let v=i32::try_from(s+a-paddr).map_err(|_|AxError::InvalidExecutable)?;code[o..o+4].copy_from_slice(&v.to_le_bytes())}, 10=>{range(code,o,4)?;let v=u32::try_from(s+a).map_err(|_|AxError::InvalidExecutable)?;code[o..o+4].copy_from_slice(&v.to_le_bytes())}, 11=>{range(code,o,4)?;let v=i32::try_from(s+a).map_err(|_|AxError::InvalidExecutable)?;code[o..o+4].copy_from_slice(&v.to_le_bytes())}, _=>return Err(LinuxError::ENOEXEC.into()) } } } Ok(())
}

fn prepare(b:&[u8])->AxResult<PreparedModule>{
    let ss=sections(b)?; let ci=ss.iter().position(|s|s.typ==SHT_PROGBITS&&s.flags&SHF_EXECINSTR!=0).ok_or(AxError::InvalidExecutable)?; let cs=ss[ci]; if cs.size==0{return Err(AxError::InvalidExecutable)}; let tab=*ss.iter().find(|s|s.typ==SHT_SYMTAB).ok_or(AxError::InvalidExecutable)?; let strs=*ss.get(tab.link as usize).ok_or(AxError::InvalidExecutable)?; if tab.entsize!=ELF64_SYM||tab.size%ELF64_SYM!=0{return Err(AxError::InvalidExecutable)};
    let(mut init,mut exit)=(None,None); for i in 0..tab.size/ELF64_SYM {let s=symbol(b,tab,i)?;if s.section as usize!=ci||s.info&15!=STT_FUNC{continue}match name(b,strs,s)?{b"thekernel_module_init"=>init=Some(s.value),b"thekernel_module_exit"=>exit=Some(s.value),_=>{}}} let init=init.ok_or(AxError::InvalidExecutable)?;if init>=cs.size||exit.is_some_and(|x|x>=cs.size){return Err(AxError::InvalidExecutable)};
    let mut w=jit_memory::prepare(cs.size).map_err(memory_error)?; let mut code=range(b,cs.offset,cs.size)?.to_vec(); relocate(&mut code,w.code_address(),b,&ss,ci)?; w.write(0,&code)?; let code=w.publish(init).map_err(memory_error)?; Ok(PreparedModule{name:String::from("module"),code,init,exit})
}

pub fn sys_init_module<M:UserMemory+?Sized>(memory:&mut UserMemoryContext<'_,M>,image:*const u8,len:usize,_args:*const c_char)->AxResult<isize>{if !has_module_capability(){return Err(AxError::OperationNotPermitted)}if len==0||len>MAX_MODULE_BYTES{return Err(AxError::InvalidInput)}let bytes=vm_load(memory,image,len).map_err(map_usercopy_error)?;let prepared=prepare(&bytes)?;if prepared.code.execute_module_entry(prepared.init)!=0{return Err(AxError::InvalidInput)}MODULES.lock().push(prepared);Ok(0)}
pub fn sys_finit_module<M:UserMemory+?Sized>(_memory:&mut UserMemoryContext<'_,M>,_fd:i32,_args:*const c_char,_flags:u32)->AxResult<isize>{if !has_module_capability(){return Err(AxError::OperationNotPermitted)}Err(AxError::OperationNotSupported)}
pub fn sys_delete_module<M:UserMemory+?Sized>(memory:&mut UserMemoryContext<'_,M>,name:*const c_char,_flags:u32)->AxResult<isize>{if !has_module_capability(){return Err(AxError::OperationNotPermitted)}let name=vm_load_until_nul_bounded(memory,name.cast(),MODULE_NAME_MAX+1).map_err(map_usercopy_error)?;let name=core::str::from_utf8(&name).map_err(|_|AxError::IllegalBytes)?;let mut modules=MODULES.lock();let index=modules.iter().position(|m|m.name==name).ok_or(LinuxError::ENOENT)?;let module=modules.remove(index);if let Some(exit)=module.exit{let _=module.code.execute_module_entry(exit);}module.code.retire().map_err(memory_error)?;Ok(0)}
