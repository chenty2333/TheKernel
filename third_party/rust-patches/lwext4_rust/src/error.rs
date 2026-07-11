use core::{
    error::Error,
    fmt::{Debug, Display},
};

use crate::ffi::EOK;

pub type Ext4Result<T = ()> = Result<T, Ext4Error>;

pub struct Ext4Error {
    pub code: i32,
    pub context: Option<&'static str>,
    /// The failed operation may already have changed filesystem metadata.
    ///
    /// This is deliberately sticky across added error context.  Callers that
    /// own the filesystem must stop issuing metadata mutations once it is
    /// observed; blindly retrying or rolling back may otherwise double-free
    /// blocks after a partially completed C operation.
    pub(crate) metadata_may_have_changed: bool,
}
impl Ext4Error {
    pub fn new(code: i32, context: impl Into<Option<&'static str>>) -> Self {
        Ext4Error {
            code,
            context: context.into(),
            metadata_may_have_changed: false,
        }
    }

    pub(crate) fn with_metadata_may_have_changed(mut self, changed: bool) -> Self {
        self.metadata_may_have_changed |= changed;
        self
    }

    /// Returns whether the failed operation may already have mutated metadata.
    pub const fn metadata_may_have_changed(&self) -> bool {
        self.metadata_may_have_changed
    }
}

impl From<i32> for Ext4Error {
    fn from(code: i32) -> Self {
        Ext4Error::new(code, None)
    }
}

impl Display for Ext4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(context) = self.context {
            write!(f, "ext4 error {}: {context}", self.code)
        } else {
            write!(f, "ext4 error {}", self.code)
        }
    }
}

impl Debug for Ext4Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Display::fmt(self, f)
    }
}

impl Error for Ext4Error {}

pub(crate) trait Context<T> {
    fn context(self, context: &'static str) -> Result<T, Ext4Error>;
}
impl Context<()> for i32 {
    fn context(self, context: &'static str) -> Result<(), Ext4Error> {
        if self != EOK as _ {
            Err(Ext4Error::new(self, Some(context)))
        } else {
            Ok(())
        }
    }
}
impl<T> Context<T> for Ext4Result<T> {
    fn context(self, context: &'static str) -> Result<T, Ext4Error> {
        self.map_err(|mut error| {
            error.context = Some(context);
            error
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_mutation_marker_is_sticky_across_context() {
        let error = Err::<(), _>(Ext4Error::new(5, "inner").with_metadata_may_have_changed(true))
            .context("outer")
            .unwrap_err();

        assert_eq!(error.context, Some("outer"));
        assert!(error.metadata_may_have_changed());
        assert!(
            error
                .with_metadata_may_have_changed(false)
                .metadata_may_have_changed()
        );
    }
}
