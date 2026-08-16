mod futex;
mod membarrier;
#[cfg(target_arch = "x86_64")]
mod rseq;

#[cfg(target_arch = "x86_64")]
pub use self::rseq::*;
pub use self::{futex::*, membarrier::*};
