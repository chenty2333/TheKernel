//! Native x86-64 ET_REL module admission.
use alloc::{string::String, vec, vec::Vec};
use core::ffi::c_char;

use axerrno::{AxError, AxResult, LinuxError};
use axtask::current;
use linux_raw_sys::general::{CAP_SYS_MODULE, O_ACCMODE, O_NONBLOCK, O_TRUNC, O_WRONLY};
use spin::Lazy;
use thekernel_linux_usercopy::{UserMemory, UserMemoryContext, vm_load, vm_load_until_nul_bounded};

use crate::{
    file::{File, FileLike, get_typed_file},
    jit_memory::{self, ExecutableCode, MemoryError},
    mm::map_usercopy_error,
    task::AsThread,
};
const MAX: usize = 16 * 1024 * 1024;
const ARGMAX: usize = 4096;
const NAMEMAX: usize = 56;
const EH: usize = 64;
const SH: usize = 64;
const SYM: usize = 24;
const RELA: usize = 24;
const ETREL: u16 = 1;
const X64: u16 = 62;
const PROG: u32 = 1;
const SYMTAB: u32 = 2;
const STRTAB: u32 = 3;
const RELOC: u32 = 4;
const NOBITS: u32 = 8;
const ALLOC: u64 = 2;
const EXEC: u64 = 4;
const UNDEF: u16 = 0;
const FUNC: u8 = 2;
#[derive(Clone, Copy)]
struct S {
    n: u32,
    t: u32,
    f: u64,
    o: usize,
    z: usize,
    l: u32,
    i: u32,
    a: usize,
    e: usize,
}
#[derive(Clone, Copy)]
struct Y {
    n: u32,
    i: u8,
    s: u16,
    v: usize,
    z: usize,
}
#[derive(Clone, Copy)]
enum P {
    C(usize),
    D(usize),
}
enum State {
    Coming,
    Live(M),
    Going,
}
struct Slot {
    name: String,
    state: State,
    refs: u32,
    deps: u32,
}
struct M {
    name: String,
    code: ExecutableCode,
    data: Vec<u8>,
    charps: Vec<Vec<u8>>,
    init: usize,
    exit: Option<usize>,
}
static MODULES: Lazy<spin::Mutex<Vec<Slot>>> = Lazy::new(|| spin::Mutex::new(Vec::new()));
fn no<T>() -> AxResult<T> {
    Err(LinuxError::ENOEXEC.into())
}
fn u16x(b: &[u8], o: usize) -> AxResult<u16> {
    b.get(o..o.checked_add(2).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn u32x(b: &[u8], o: usize) -> AxResult<u32> {
    b.get(o..o.checked_add(4).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn u64x(b: &[u8], o: usize) -> AxResult<u64> {
    b.get(o..o.checked_add(8).ok_or(AxError::InvalidExecutable)?)
        .and_then(|x| x.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(AxError::InvalidExecutable)
}
fn sl(b: &[u8], o: usize, z: usize) -> AxResult<&[u8]> {
    b.get(o..o.checked_add(z).ok_or(AxError::InvalidExecutable)?)
        .ok_or(AxError::InvalidExecutable)
}
fn cs(b: &[u8], o: usize) -> AxResult<&[u8]> {
    let x = b.get(o..).ok_or(AxError::InvalidExecutable)?;
    Ok(&x[..x
        .iter()
        .position(|x| *x == 0)
        .ok_or(AxError::InvalidExecutable)?])
}
fn al(x: usize, a: usize) -> AxResult<usize> {
    if a == 0 || !a.is_power_of_two() {
        return no();
    }
    x.checked_add(a - 1)
        .map(|x| x & !(a - 1))
        .ok_or(AxError::InvalidExecutable)
}
fn cap() -> bool {
    current()
        .as_thread()
        .has_effective_capability(CAP_SYS_MODULE)
}
fn me(e: MemoryError) -> AxError {
    match e {
        MemoryError::Unavailable(x) | MemoryError::Quarantined(x) | MemoryError::Retained(x) => x,
    }
}
fn sh(b: &[u8]) -> AxResult<Vec<S>> {
    if b.len() < EH
        || &b[..4] != b"\x7fELF"
        || b[4] != 2
        || b[5] != 1
        || b[6] != 1
        || u16x(b, 16)? != ETREL
        || u16x(b, 18)? != X64
    {
        return no();
    }
    let o = usize::try_from(u64x(b, 40)?).map_err(|_| AxError::InvalidExecutable)?;
    let e = usize::from(u16x(b, 58)?);
    let n = usize::from(u16x(b, 60)?);
    if e != SH || n == 0 {
        return no();
    }
    sl(b, o, e.checked_mul(n).ok_or(AxError::InvalidExecutable)?)?;
    (0..n)
        .map(|j| {
            let p = o + j * SH;
            Ok(S {
                n: u32x(b, p)?,
                t: u32x(b, p + 4)?,
                f: u64x(b, p + 8)?,
                o: usize::try_from(u64x(b, p + 24)?).map_err(|_| AxError::InvalidExecutable)?,
                z: usize::try_from(u64x(b, p + 32)?).map_err(|_| AxError::InvalidExecutable)?,
                l: u32x(b, p + 40)?,
                i: u32x(b, p + 44)?,
                a: usize::try_from(u64x(b, p + 48)?).map_err(|_| AxError::InvalidExecutable)?,
                e: usize::try_from(u64x(b, p + 56)?).map_err(|_| AxError::InvalidExecutable)?,
            })
        })
        .collect()
}
fn sy(b: &[u8], t: S, j: usize) -> AxResult<Y> {
    if t.e != SYM || j >= t.z / SYM {
        return no();
    }
    let p = t.o.checked_add(j * SYM).ok_or(AxError::InvalidExecutable)?;
    sl(b, p, SYM)?;
    Ok(Y {
        n: u32x(b, p)?,
        i: b[p + 4],
        s: u16x(b, p + 6)?,
        v: usize::try_from(u64x(b, p + 8)?).map_err(|_| AxError::InvalidExecutable)?,
        z: usize::try_from(u64x(b, p + 16)?).map_err(|_| AxError::InvalidExecutable)?,
    })
}
fn pa(p: P, cb: usize, db: usize) -> AxResult<usize> {
    match p {
        P::C(x) => cb.checked_add(x),
        P::D(x) => db.checked_add(x),
    }
    .ok_or(AxError::InvalidExecutable)
}
fn put(c: &mut [u8], d: &mut [u8], p: P, v: &[u8]) -> AxResult<()> {
    match p {
        P::C(x) => c.get_mut(x..x + v.len()),
        P::D(x) => d.get_mut(x..x + v.len()),
    }
    .ok_or(AxError::InvalidExecutable)?
    .copy_from_slice(v);
    Ok(())
}
fn rel(
    b: &[u8],
    ss: &[S],
    ps: &[Option<P>],
    c: &mut [u8],
    d: &mut [u8],
    cb: usize,
) -> AxResult<()> {
    let db = d.as_ptr() as usize;
    for r in ss.iter().filter(|x| x.t == RELOC) {
        let dst = ps
            .get(r.i as usize)
            .ok_or(AxError::InvalidExecutable)?
            .ok_or(AxError::InvalidExecutable)?;
        let tab = *ss.get(r.l as usize).ok_or(AxError::InvalidExecutable)?;
        if tab.t != SYMTAB || r.e != RELA || r.z % RELA != 0 {
            return no();
        }
        for j in 0..r.z / RELA {
            let q =
                r.o.checked_add(j.checked_mul(RELA).ok_or(AxError::InvalidExecutable)?)
                    .ok_or(AxError::InvalidExecutable)?;
            sl(b, q, RELA)?;
            let off = usize::try_from(u64x(b, q)?).map_err(|_| AxError::InvalidExecutable)?;
            let inf = u64x(b, q + 8)?;
            let y = sy(
                b,
                tab,
                usize::try_from(inf >> 32).map_err(|_| AxError::InvalidExecutable)?,
            )?;
            if y.s == UNDEF {
                return no();
            }
            let sec = *ss.get(y.s as usize).ok_or(AxError::InvalidExecutable)?;
            let sp = ps
                .get(y.s as usize)
                .ok_or(AxError::InvalidExecutable)?
                .ok_or(AxError::InvalidExecutable)?;
            if y.v > sec.z || y.z > sec.z - y.v {
                return no();
            }
            let dp = match dst {
                P::C(x) => P::C(x.checked_add(off).ok_or(AxError::InvalidExecutable)?),
                P::D(x) => P::D(x.checked_add(off).ok_or(AxError::InvalidExecutable)?),
            };
            let a = u64x(b, q + 16)? as i64 as i128;
            let s = pa(sp, cb, db)? as i128;
            let p = pa(dp, cb, db)? as i128;
            match inf as u32 {
                1 => put(
                    c,
                    d,
                    dp,
                    &u64::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                2 | 4 => put(
                    c,
                    d,
                    dp,
                    &i32::try_from(s + a - p)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                10 => put(
                    c,
                    d,
                    dp,
                    &u32::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                11 => put(
                    c,
                    d,
                    dp,
                    &i32::try_from(s + a)
                        .map_err(|_| AxError::InvalidExecutable)?
                        .to_le_bytes(),
                )?,
                _ => return no(),
            }
        }
    }
    Ok(())
}
fn args(b: &[u8]) -> AxResult<Vec<(Vec<u8>, Vec<u8>)>> {
    let (mut i, mut stop) = (0, false);
    let mut out = Vec::new();
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1
        }
        let mut x = Vec::new();
        let mut q = 0;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            let z = b[i];
            i += 1;
            if q != 0 {
                if z == q {
                    q = 0
                } else if z == b'\\' && i < b.len() {
                    x.push(b[i]);
                    i += 1
                } else {
                    x.push(z)
                }
            } else if z == b'\'' || z == b'"' {
                q = z
            } else {
                x.push(z)
            }
        }
        if q != 0 {
            return Err(LinuxError::EINVAL.into());
        }
        if x == b"--" {
            stop = true;
            continue;
        }
        if stop {
            continue;
        }
        let e = x
            .iter()
            .position(|x| *x == b'=')
            .ok_or(LinuxError::EINVAL)?;
        let mut n = x[..e].to_vec();
        while n.first() == Some(&b'-') {
            n.remove(0);
        }
        if n.is_empty() {
            return Err(LinuxError::EINVAL.into());
        }
        out.push((n, x[e + 1..].to_vec()))
    }
    Ok(out)
}
fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x == y || (*x == b'-' && *y == b'_') || (*x == b'_' && *y == b'-'))
}
fn num(x: &[u8], sg: bool, bits: u32) -> AxResult<u64> {
    let s = core::str::from_utf8(x).map_err(|_| LinuxError::EINVAL)?;
    if sg {
        let neg = s.starts_with('-');
        let t = s.trim_start_matches(['+', '-']);
        let (t, r) = if let Some(t) = t.strip_prefix("0x") {
            (t, 16)
        } else if t.len() > 1 && t.starts_with('0') {
            (&t[1..], 8)
        } else {
            (t, 10)
        };
        let mut v = i128::from_str_radix(t, r).map_err(|_| LinuxError::EINVAL)?;
        if neg {
            v = -v
        };
        if v < -(1i128 << (bits - 1)) || v > (1i128 << (bits - 1)) - 1 {
            return Err(LinuxError::EINVAL.into());
        }
        Ok(v as u64)
    } else {
        let t = s.strip_prefix('+').unwrap_or(s);
        let (t, r) = if let Some(t) = t.strip_prefix("0x") {
            (t, 16)
        } else if t.len() > 1 && t.starts_with('0') {
            (&t[1..], 8)
        } else {
            (t, 10)
        };
        let v = u128::from_str_radix(t, r).map_err(|_| LinuxError::EINVAL)?;
        if v > (1u128 << bits) - 1 {
            return Err(LinuxError::EINVAL.into());
        }
        Ok(v as u64)
    }
}
fn off(d: &[u8], p: usize, n: usize) -> AxResult<usize> {
    let b = d.as_ptr() as usize;
    if p < b || p.checked_add(n).ok_or(AxError::InvalidExecutable)? > b + d.len() {
        return no();
    }
    Ok(p - b)
}
fn params(
    d: &mut Vec<u8>,
    ss: &[S],
    ps: &[Option<P>],
    which: Option<usize>,
    av: &[(Vec<u8>, Vec<u8>)],
) -> AxResult<Vec<Vec<u8>>> {
    let Some(i) = which else {
        return Ok(Vec::new());
    };
    let s = ss[i];
    let Some(P::D(o)) = ps[i] else { return no() };
    if s.z < 16 || u32x(d, o)? != 1 || u32x(d, o + 12)? != 0 {
        return no();
    }
    let old = d.clone();
    let r = (|| {
        let rs = u32x(d, o + 4)? as usize;
        let n = u32x(d, o + 8)? as usize;
        if rs != 40 || o + 16 + n.checked_mul(rs).ok_or(AxError::InvalidExecutable)? > o + s.z {
            return no();
        }
        let mut cp = Vec::new();
        for (k, v) in av {
            let mut seen = false;
            for j in 0..n {
                let q = o + 16 + j * rs;
                let np = usize::try_from(u64x(d, q)?).map_err(|_| AxError::InvalidExecutable)?;
                if !eq(k, cs(d, off(d, np, 1)?)?) {
                    continue;
                }
                seen = true;
                let ap =
                    usize::try_from(u64x(d, q + 8)?).map_err(|_| AxError::InvalidExecutable)?;
                let cnt =
                    usize::try_from(u64x(d, q + 16)?).map_err(|_| AxError::InvalidExecutable)?;
                let kind = u16x(d, q + 24)?;
                let fl = u16x(d, q + 26)?;
                let cap = u32x(d, q + 28)? as usize;
                if fl & !1 != 0 || u32x(d, q + 32)? != 0 || cap == 0 {
                    return no();
                }
                let vs: Vec<&[u8]> = if fl & 1 != 0 {
                    v.split(|x| *x == b',').collect()
                } else {
                    vec![v]
                };
                if vs.len() > cap {
                    return Err(LinuxError::EINVAL.into());
                }
                let w = match kind {
                    0 => 1,
                    1 | 2 => 4,
                    3 | 4 | 6 => 8,
                    5 => cap,
                    _ => return no(),
                };
                let ao = off(
                    d,
                    ap,
                    if kind == 5 {
                        cap
                    } else {
                        w.checked_mul(cap).ok_or(AxError::InvalidExecutable)?
                    },
                )?;
                if cnt != 0 {
                    off(d, cnt, 4)?;
                }
                for (i, x) in vs.iter().enumerate() {
                    let z = ao + i * w;
                    match kind {
                        0 => {
                            d[z] = match *x {
                                b"1" | b"y" | b"Y" | b"yes" | b"true" | b"on" => 1,
                                b"0" | b"n" | b"N" | b"no" | b"false" | b"off" => 0,
                                _ => return Err(LinuxError::EINVAL.into()),
                            }
                        }
                        1 => d[z..z + 4].copy_from_slice(&(num(x, true, 32)? as u32).to_le_bytes()),
                        2 => {
                            d[z..z + 4].copy_from_slice(&(num(x, false, 32)? as u32).to_le_bytes())
                        }
                        3 => d[z..z + 8].copy_from_slice(&num(x, true, 64)?.to_le_bytes()),
                        4 => d[z..z + 8].copy_from_slice(&num(x, false, 64)?.to_le_bytes()),
                        5 => {
                            if x.len() >= cap {
                                return Err(LinuxError::EINVAL.into());
                            }
                            d[z..z + cap].fill(0);
                            d[z..z + x.len()].copy_from_slice(x)
                        }
                        6 => {
                            let mut s = Vec::new();
                            s.try_reserve_exact(x.len() + 1)
                                .map_err(|_| AxError::NoMemory)?;
                            s.extend_from_slice(x);
                            s.push(0);
                            d[z..z + 8].copy_from_slice(&(s.as_ptr() as u64).to_le_bytes());
                            cp.push(s)
                        }
                        _ => unreachable!(),
                    }
                }
                if cnt != 0 {
                    let x = off(d, cnt, 4)?;
                    d[x..x + 4].copy_from_slice(&(vs.len() as u32).to_le_bytes())
                }
            }
            if !seen {
                return Err(LinuxError::EINVAL.into());
            }
        }
        Ok(cp)
    })();
    if r.is_err() {
        d.copy_from_slice(&old)
    }
    r
}
fn prep(b: &[u8], av: &[(Vec<u8>, Vec<u8>)]) -> AxResult<M> {
    let ss = sh(b)?;
    let names = *ss
        .get(usize::from(u16x(b, 62)?))
        .ok_or(AxError::InvalidExecutable)?;
    if names.t != STRTAB {
        return no();
    }
    let names = sl(b, names.o, names.z)?;
    for s in &ss {
        cs(names, s.n as usize)?;
    }
    let param_section = ss
        .iter()
        .position(|s| cs(names, s.n as usize).is_ok_and(|x| x == b".thekernel.param.v1"));
    let tab = *ss
        .iter()
        .find(|x| x.t == SYMTAB)
        .ok_or(AxError::InvalidExecutable)?;
    let strtab = *ss.get(tab.l as usize).ok_or(AxError::InvalidExecutable)?;
    if tab.e != SYM || tab.z % SYM != 0 || strtab.t != STRTAB {
        return no();
    }
    let mut ps = Vec::new();
    ps.resize(ss.len(), None);
    let (mut cl, mut dl) = (0, 0);
    for (i, s) in ss.iter().enumerate() {
        if s.f & ALLOC == 0 {
            continue;
        }
        if s.f & EXEC != 0 {
            cl = al(cl, s.a.max(1))?;
            ps[i] = Some(P::C(cl));
            cl = cl.checked_add(s.z).ok_or(AxError::InvalidExecutable)?
        } else {
            dl = al(dl, s.a.max(1))?;
            ps[i] = Some(P::D(dl));
            dl = dl.checked_add(s.z).ok_or(AxError::InvalidExecutable)?
        }
    }
    if cl == 0 {
        return no();
    }
    let mut c = Vec::new();
    c.try_reserve_exact(cl).map_err(|_| AxError::NoMemory)?;
    c.resize(cl, 0);
    let mut d = Vec::new();
    d.try_reserve_exact(dl).map_err(|_| AxError::NoMemory)?;
    d.resize(dl, 0);
    for (i, s) in ss.iter().enumerate() {
        let Some(p) = ps[i] else { continue };
        if s.t != PROG && s.t != NOBITS {
            return no();
        }
        if s.t == PROG {
            match p {
                P::C(o) => c[o..o + s.z].copy_from_slice(sl(b, s.o, s.z)?),
                P::D(o) => d[o..o + s.z].copy_from_slice(sl(b, s.o, s.z)?),
            }
        }
    }
    let (mut init, mut exit) = (None, None);
    for j in 0..tab.z / SYM {
        let y = sy(b, tab, j)?;
        if y.s == UNDEF {
            continue;
        }
        let sec = *ss.get(y.s as usize).ok_or(AxError::InvalidExecutable)?;
        if y.v > sec.z || y.z > sec.z - y.v {
            return no();
        }
        if y.i & 15 == FUNC {
            if let Some(P::C(o)) = ps[y.s as usize] {
                match cs(sl(b, strtab.o, strtab.z)?, y.n as usize)? {
                    b"thekernel_module_init" => {
                        init = Some(o.checked_add(y.v).ok_or(AxError::InvalidExecutable)?)
                    }
                    b"thekernel_module_exit" => {
                        exit = Some(o.checked_add(y.v).ok_or(AxError::InvalidExecutable)?)
                    }
                    _ => {}
                }
            }
        }
    }
    let init = init.ok_or(AxError::InvalidExecutable)?;
    let mi = ss
        .iter()
        .find(|s| cs(names, s.n as usize).is_ok_and(|x| x == b".modinfo"))
        .ok_or(AxError::InvalidExecutable)?;
    let mn = sl(b, mi.o, mi.z)?
        .split(|x| *x == 0)
        .find_map(|x| x.strip_prefix(b"name="))
        .ok_or(AxError::InvalidExecutable)?;
    if mn.is_empty() || mn.len() > NAMEMAX {
        return no();
    }
    let name = core::str::from_utf8(mn)
        .map_err(|_| AxError::IllegalBytes)?
        .into();
    let mut w = jit_memory::prepare(cl).map_err(me)?;
    rel(b, &ss, &ps, &mut c, &mut d, w.code_address())?;
    let charps = params(&mut d, &ss, &ps, param_section, av)?;
    w.write(0, &c)?;
    Ok(M {
        name,
        code: w.publish(init).map_err(me)?,
        data: d,
        charps,
        init,
        exit,
    })
}
fn activate(x: M) -> AxResult<isize> {
    let n = x.name.clone();
    {
        let mut v = MODULES.lock();
        if v.iter().any(|x| x.name == n) {
            return Err(LinuxError::EEXIST.into());
        }
        v.try_reserve(1).map_err(|_| AxError::NoMemory)?;
        v.push(Slot {
            name: n.clone(),
            state: State::Coming,
            refs: 1,
            deps: 0,
        })
    }
    let r = x.code.execute_module_entry(x.init);
    let mut v = MODULES.lock();
    let i = v
        .iter()
        .position(|x| x.name == n)
        .ok_or(AxError::BadState)?;
    if r != 0 {
        v.remove(i);
        return Err(if r < 0 {
            LinuxError::try_from(-r).unwrap_or(LinuxError::EINVAL)
        } else {
            LinuxError::EINVAL
        }
        .into());
    }
    v[i].state = State::Live(x);
    Ok(0)
}
fn ua<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const c_char,
) -> AxResult<Vec<(Vec<u8>, Vec<u8>)>> {
    if p.is_null() {
        Ok(Vec::new())
    } else {
        args(&vm_load_until_nul_bounded(m, p.cast(), ARGMAX).map_err(map_usercopy_error)?)
    }
}
pub fn sys_init_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const u8,
    n: usize,
    a: *const c_char,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if n == 0 || n > MAX {
        return Err(AxError::InvalidInput);
    }
    let a = ua(m, a)?;
    activate(prep(&vm_load(m, p, n).map_err(map_usercopy_error)?, &a)?)
}
pub fn sys_finit_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    fd: i32,
    a: *const c_char,
    fl: u32,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if fl & !7 != 0 {
        return Err(AxError::InvalidInput);
    }
    if fl != 0 {
        return Err(AxError::OperationNotSupported);
    }
    let a = ua(m, a)?;
    let f = get_typed_file::<File>(fd)?;
    f.check_io_access()?;
    if f.status_flags() & O_ACCMODE == O_WRONLY {
        return Err(AxError::BadFileDescriptor);
    }
    let n = usize::try_from(f.stat()?.size).map_err(|_| AxError::InvalidInput)?;
    if n == 0 || n > MAX {
        return Err(AxError::InvalidInput);
    }
    let mut b = Vec::new();
    b.try_reserve_exact(n).map_err(|_| AxError::NoMemory)?;
    b.resize(n, 0);
    if f.inner().read_at(&mut b, 0)? != n {
        return no();
    }
    activate(prep(&b, &a)?)
}
pub fn sys_delete_module<Mm: UserMemory + ?Sized>(
    m: &mut UserMemoryContext<'_, Mm>,
    p: *const c_char,
    fl: u32,
) -> AxResult<isize> {
    if !cap() {
        return Err(AxError::OperationNotPermitted);
    }
    if fl & !(O_NONBLOCK | O_TRUNC) != 0 {
        return Err(LinuxError::EINVAL.into());
    }
    let raw = vm_load_until_nul_bounded(m, p.cast(), NAMEMAX + 1).map_err(map_usercopy_error)?;
    let n = core::str::from_utf8(&raw).map_err(|_| AxError::IllegalBytes)?;
    let x = {
        let mut v = MODULES.lock();
        let i = v
            .iter()
            .position(|x| x.name == n)
            .ok_or(LinuxError::ENOENT)?;
        if !matches!(v[i].state, State::Live(_)) || v[i].refs != 1 || v[i].deps != 0 {
            return Err(LinuxError::EBUSY.into());
        }
        match core::mem::replace(&mut v[i].state, State::Going) {
            State::Live(x) => x,
            _ => unreachable!(),
        }
    };
    if let Some(e) = x.exit {
        let _ = x.code.execute_module_entry(e);
    };
    x.code.retire().map_err(me)?;
    let mut v = MODULES.lock();
    if let Some(i) = v
        .iter()
        .position(|x| x.name == n && matches!(x.state, State::Going))
    {
        v.remove(i);
    };
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn param_image() -> Vec<u8> {
        let mut data = vec![0; 96];
        data[0..4].copy_from_slice(&1u32.to_le_bytes());
        data[4..8].copy_from_slice(&40u32.to_le_bytes());
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        let base = data.as_ptr() as usize;
        data[16..24].copy_from_slice(&((base + 64) as u64).to_le_bytes());
        data[24..32].copy_from_slice(&((base + 72) as u64).to_le_bytes());
        data[32..40].copy_from_slice(&((base + 80) as u64).to_le_bytes());
        data[40..42].copy_from_slice(&1u16.to_le_bytes());
        data[44..48].copy_from_slice(&1u32.to_le_bytes());
        data[64..68].copy_from_slice(b"foo\0");
        data
    }

    #[test]
    fn parameters_write_data_and_rollback_as_one_transaction() {
        let mut data = param_image();
        let section = S {
            n: 0,
            t: PROG,
            f: ALLOC,
            o: 0,
            z: data.len(),
            l: 0,
            i: 0,
            a: 1,
            e: 0,
        };
        let args = vec![(b"foo".to_vec(), b"0x2a".to_vec())];
        params(&mut data, &[section], &[Some(P::D(0))], Some(0), &args).unwrap();
        assert_eq!(&data[72..76], &42u32.to_le_bytes());
        let before = data.clone();
        let bad = vec![
            (b"foo".to_vec(), b"7".to_vec()),
            (b"unknown".to_vec(), b"1".to_vec()),
        ];
        assert!(params(&mut data, &[section], &[Some(P::D(0))], Some(0), &bad).is_err());
        assert_eq!(data, before);
    }

    #[test]
    fn module_arguments_handle_quotes_dash_and_underscore() {
        let got = args(b"--foo-bar='quoted value' -- answer=ignored").unwrap();
        assert_eq!(got, vec![(b"foo-bar".to_vec(), b"quoted value".to_vec())]);
        assert!(eq(b"foo-bar", b"foo_bar"));
        assert_eq!(num(b"077", false, 32).unwrap(), 63);
    }
}
