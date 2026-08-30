mod acct;
mod clone;
mod clone3;
mod ctl;
mod execve;
mod exit;
mod ioport;
mod job;
mod kexec;
mod keys;
mod module;
mod ptrace;
mod schedule;
mod thread;
mod wait;

pub use self::{
    acct::*, clone::*, clone3::*, ctl::*, execve::*, exit::*, ioport::*, job::*, kexec::*, keys::*,
    module::*, ptrace::*, schedule::*, thread::*, wait::*,
};
