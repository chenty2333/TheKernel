//! Opaque Linux pathname and directory-entry name types.
//!
//! Path resolution operates on bytes. UTF-8 belongs only in adapters that
//! need an encoding, such as FAT; ext4 and tmpfs must pass bytes unchanged.

use alloc::{borrow::ToOwned, sync::Arc, vec::Vec};
use core::{borrow::Borrow, fmt, ops::Deref};

use crate::{VfsError, VfsResult};

pub const DOT: &FsName = FsName::new(b".");
pub const DOTDOT: &FsName = FsName::new(b"..");
pub const MAX_NAME_LEN: usize = 255;

#[repr(transparent)]
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsName([u8]);

impl FsName {
    pub const fn new(bytes: &[u8]) -> &Self {
        // SAFETY: FsName is transparent over [u8].
        unsafe { &*(bytes as *const [u8] as *const Self) }
    }
    pub const fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub const fn len(&self) -> usize {
        self.0.len()
    }
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for FsName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FsName").field(&&self.0).finish()
    }
}
impl ToOwned for FsName {
    type Owned = FsNameBuf;
    fn to_owned(&self) -> FsNameBuf {
        FsNameBuf(self.0.to_owned())
    }
}
impl AsRef<FsName> for FsName {
    fn as_ref(&self) -> &FsName {
        self
    }
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsNameBuf(Vec<u8>);
impl FsNameBuf {
    pub const fn new() -> Self {
        Self(Vec::new())
    }
    pub fn from_vec(bytes: Vec<u8>) -> VfsResult<Self> {
        verify_entry_name(FsName::new(&bytes))?;
        Ok(Self(bytes))
    }
    /// Owns one of the synthetic dot entries emitted by `readdir`.
    ///
    /// Normal lookup and creation names must continue through [`Self::from_vec`]
    /// and therefore reject `.` and `..`.  Directory implementations use this
    /// narrow constructor only for their internal, enumerable dot entries.
    pub fn from_readdir_pseudo_vec(bytes: Vec<u8>) -> VfsResult<Self> {
        let name = FsName::new(&bytes);
        if name.as_bytes().contains(&b'/') || name.as_bytes().contains(&0) {
            return Err(VfsError::InvalidInput);
        }
        if name.len() > MAX_NAME_LEN {
            return Err(VfsError::NameTooLong);
        }
        if name != DOT && name != DOTDOT {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self(bytes))
    }
    pub fn as_name(&self) -> &FsName {
        FsName::new(&self.0)
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }
}
impl Deref for FsNameBuf {
    type Target = FsName;
    fn deref(&self) -> &FsName {
        self.as_name()
    }
}
impl Borrow<FsName> for FsNameBuf {
    fn borrow(&self) -> &FsName {
        self
    }
}
impl AsRef<FsName> for FsNameBuf {
    fn as_ref(&self) -> &FsName {
        self
    }
}

pub(crate) fn verify_entry_name(name: &FsName) -> VfsResult<()> {
    if name == DOT
        || name == DOTDOT
        || name.as_bytes().contains(&b'/')
        || name.as_bytes().contains(&0)
    {
        return Err(VfsError::InvalidInput);
    }
    if name.as_bytes().len() > MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;

    #[test]
    fn readdir_pseudo_names_allow_only_dot_entries() {
        assert_eq!(
            FsNameBuf::from_readdir_pseudo_vec(b".".to_vec())
                .unwrap()
                .as_bytes(),
            b"."
        );
        assert_eq!(
            FsNameBuf::from_readdir_pseudo_vec(b"..".to_vec())
                .unwrap()
                .as_bytes(),
            b".."
        );
        assert!(FsNameBuf::from_vec(b".".to_vec()).is_err());
        assert!(FsNameBuf::from_vec(b"..".to_vec()).is_err());
    }

    #[test]
    fn readdir_pseudo_names_reject_invalid_bytes() {
        assert!(FsNameBuf::from_readdir_pseudo_vec(b"./".to_vec()).is_err());
        assert!(FsNameBuf::from_readdir_pseudo_vec(b".\0".to_vec()).is_err());
        assert!(FsNameBuf::from_readdir_pseudo_vec(vec![b'.'; MAX_NAME_LEN + 1]).is_err());
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Component<'a> {
    RootDir,
    CurDir,
    ParentDir,
    Normal(&'a FsName),
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum FinalComponentKind<'a> {
    Normal(&'a FsName),
    Dot,
    DotDot,
    Root,
}
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct FinalComponent<'a> {
    kind: FinalComponentKind<'a>,
    requires_directory: bool,
}
impl<'a> FinalComponent<'a> {
    const fn new(kind: FinalComponentKind<'a>, requires_directory: bool) -> Self {
        Self {
            kind,
            requires_directory,
        }
    }
    pub const fn kind(self) -> FinalComponentKind<'a> {
        self.kind
    }
    pub const fn requires_directory(self) -> bool {
        self.requires_directory
    }
}

pub struct Components<'a> {
    path: &'a [u8],
    at_start: bool,
}
impl<'a> Components<'a> {
    pub fn as_path(&self) -> &'a FsPath {
        FsPath::new(self.path)
    }
    fn parse(component: &'a [u8], terminal: bool, at_start: bool) -> Option<Component<'a>> {
        match component {
            b"" if at_start && terminal => Some(Component::RootDir),
            b"" => None,
            // Dot still performs directory/search admission. Dropping it
            // lets a retained directory handle bypass lifecycle checks.
            b"." => Some(Component::CurDir),
            b".." => Some(Component::ParentDir),
            bytes => Some(Component::Normal(FsName::new(bytes))),
        }
    }
}
impl<'a> Iterator for Components<'a> {
    type Item = Component<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.path.is_empty() {
                return None;
            }
            // An absolute path starts at the root component. Splitting on '/'
            // alone would silently drop the leading separator and resolve the
            // remaining components against the current directory instead.
            if self.at_start && self.path[0] == b'/' {
                self.at_start = false;
                self.path = &self.path[1..];
                return Some(Component::RootDir);
            }
            let split = self.path.iter().position(|b| *b == b'/');
            let (component, rest) = match split {
                Some(n) => (&self.path[..n], &self.path[n + 1..]),
                None => (self.path, &[][..]),
            };
            self.path = rest;
            // Back iteration must not clear `at_start`: resolvers pop the
            // final component with `next_back` and then continue the walk in
            // front order, which still has to observe the root component.
            let at_start = self.at_start;
            self.at_start = false;
            if let Some(component) = Self::parse(component, false, at_start) {
                return Some(component);
            }
        }
    }
}
impl<'a> DoubleEndedIterator for Components<'a> {
    fn next_back(&mut self) -> Option<Self::Item> {
        loop {
            if self.path.is_empty() {
                return None;
            }
            let split = self.path.iter().rposition(|b| *b == b'/');
            let (component, rest) = match split {
                // Keep the root after removing the only named component
                // of an absolute path. A later front or back step must
                // still resolve from the root rather than the current dir.
                Some(0) if self.at_start && self.path.len() > 1 => {
                    (&self.path[1..], &self.path[..1])
                }
                Some(n) => (&self.path[n + 1..], &self.path[..n]),
                None => (self.path, &[][..]),
            };
            self.path = rest;
            if let Some(component) = Self::parse(component, rest.is_empty(), self.at_start) {
                return Some(component);
            }
        }
    }
}

#[repr(transparent)]
#[derive(Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsPath([u8]);
impl FsPath {
    pub const fn new(bytes: &[u8]) -> &Self {
        unsafe { &*(bytes as *const [u8] as *const Self) }
    }
    pub const fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn components(&self) -> Components<'_> {
        Components {
            path: &self.0,
            at_start: true,
        }
    }
    pub fn file_name(&self) -> Option<&FsName> {
        self.components().next_back().and_then(|c| match c {
            Component::Normal(name) => Some(name),
            _ => None,
        })
    }
    pub fn split_final_component(&self) -> Option<(&FsPath, FinalComponent<'_>)> {
        let raw = self.as_bytes();
        if raw.is_empty() {
            return None;
        }
        let trimmed = raw
            .iter()
            .rposition(|b| *b != b'/')
            .map(|n| &raw[..=n])
            .unwrap_or(&[]);
        if trimmed.is_empty() {
            return Some((
                FsPath::new(raw),
                FinalComponent::new(FinalComponentKind::Root, true),
            ));
        }
        let start = trimmed
            .iter()
            .rposition(|b| *b == b'/')
            .map_or(0, |n| n + 1);
        let name = &trimmed[start..];
        let kind = match name {
            b"." => FinalComponentKind::Dot,
            b".." => FinalComponentKind::DotDot,
            _ => FinalComponentKind::Normal(FsName::new(name)),
        };
        Some((
            FsPath::new(&raw[..start]),
            FinalComponent::new(
                kind,
                trimmed.len() != raw.len() || !matches!(kind, FinalComponentKind::Normal(_)),
            ),
        ))
    }
    pub fn join(&self, other: impl AsRef<FsPath>) -> FsPathBuf {
        let mut path = self.to_owned();
        path.push(other);
        path
    }
    pub fn parent(&self) -> Option<&FsPath> {
        let mut components = self.components();
        match components.next_back() {
            Some(Component::Normal(_) | Component::CurDir | Component::ParentDir) => {
                Some(components.as_path())
            }
            _ => None,
        }
    }
    pub fn is_absolute(&self) -> bool {
        self.0.starts_with(b"/")
    }
    pub fn normalize(&self) -> Option<FsPathBuf> {
        let mut result = FsPathBuf::new();
        for component in self.components() {
            match component {
                Component::RootDir => result.push(FsPath::new(b"/")),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !result.pop() {
                        return None;
                    }
                }
                Component::Normal(name) => result.push(FsPath::new(name.as_bytes())),
            }
        }
        Some(result)
    }
}
impl fmt::Debug for FsPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FsPath").field(&&self.0).finish()
    }
}
impl ToOwned for FsPath {
    type Owned = FsPathBuf;
    fn to_owned(&self) -> FsPathBuf {
        FsPathBuf {
            inner: self.0.to_owned(),
        }
    }
}
impl From<&FsPath> for Arc<FsPath> {
    fn from(path: &FsPath) -> Self {
        let bytes: Arc<[u8]> = Arc::from(path.as_bytes());
        unsafe { Arc::from_raw(Arc::into_raw(bytes) as *const FsPath) }
    }
}
impl AsRef<FsPath> for FsPath {
    fn as_ref(&self) -> &FsPath {
        self
    }
}

// Path buffers have the same owned-byte semantics as `FsNameBuf`: cloning
// duplicates the opaque path bytes and does not re-parse or normalize them.
// This is needed for fscontext option snapshots, whose values must remain
// independent of later caller-side mutation.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct FsPathBuf {
    inner: Vec<u8>,
}
impl FsPathBuf {
    pub const fn new() -> Self {
        Self { inner: Vec::new() }
    }
    pub fn from_vec(inner: Vec<u8>) -> Self {
        Self { inner }
    }
    pub fn into_vec(self) -> Vec<u8> {
        self.inner
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    pub fn pop(&mut self) -> bool {
        match self.parent().map(|path| path.as_bytes().len()) {
            Some(len) => {
                self.inner.truncate(len);
                true
            }
            None => false,
        }
    }
    pub fn push(&mut self, path: impl AsRef<FsPath>) {
        let path = path.as_ref();
        if path.as_bytes().is_empty() {
            return;
        }
        if path.is_absolute() {
            self.inner.clear();
        } else if !self.inner.ends_with(b"/") {
            self.inner.push(b'/');
        }
        self.inner.extend_from_slice(path.as_bytes());
    }
}
impl Deref for FsPathBuf {
    type Target = FsPath;
    fn deref(&self) -> &FsPath {
        FsPath::new(&self.inner)
    }
}
impl Borrow<FsPath> for FsPathBuf {
    fn borrow(&self) -> &FsPath {
        self
    }
}
impl AsRef<FsPath> for FsPathBuf {
    fn as_ref(&self) -> &FsPath {
        self
    }
}

pub(crate) fn try_build_absolute_path<T>(
    components_leaf_to_root: &[T],
    component_name: impl Fn(&T) -> &FsName,
) -> VfsResult<FsPathBuf> {
    let mut capacity = 1usize;
    for component in components_leaf_to_root.iter().rev() {
        let name = component_name(component).as_bytes();
        if !name.is_empty() {
            capacity = capacity
                .checked_add(usize::from(capacity != 1))
                .and_then(|n| n.checked_add(name.len()))
                .ok_or(VfsError::NoMemory)?;
        }
    }
    let mut path = Vec::new();
    path.try_reserve_exact(capacity)
        .map_err(|_| VfsError::NoMemory)?;
    path.push(b'/');
    for component in components_leaf_to_root.iter().rev() {
        let name = component_name(component).as_bytes();
        if !name.is_empty() {
            if path.len() != 1 {
                path.push(b'/');
            }
            path.extend_from_slice(name);
        }
    }
    Ok(FsPathBuf::from_vec(path))
}

#[cfg(test)]
mod component_tests {
    use super::*;

    fn normal(component: Component<'_>, name: &[u8]) -> bool {
        matches!(component, Component::Normal(n) if n.as_bytes() == name)
    }

    #[test]
    fn absolute_paths_yield_root_dir_first() {
        let mut components = FsPath::new(b"/bin/sh").components();
        assert_eq!(components.next(), Some(Component::RootDir));
        assert!(normal(components.next().unwrap(), b"bin"));
        assert!(normal(components.next().unwrap(), b"sh"));
        assert_eq!(components.next(), None);
    }

    #[test]
    fn relative_paths_have_no_root_dir() {
        let mut components = FsPath::new(b"bin/sh").components();
        assert!(normal(components.next().unwrap(), b"bin"));
        assert!(normal(components.next().unwrap(), b"sh"));
        assert_eq!(components.next(), None);
    }

    #[test]
    fn dot_components_preserve_directory_search_in_both_directions() {
        for path in [b".".as_slice(), b"./", b"./."] {
            let count = if path == b"./." { 2 } else { 1 };
            let expected = (0..count)
                .map(|_| Component::CurDir)
                .collect::<alloc::vec::Vec<_>>();
            assert_eq!(
                FsPath::new(path).components().collect::<alloc::vec::Vec<_>>(),
                expected,
            );
            assert_eq!(
                FsPath::new(path).components().rev().collect::<alloc::vec::Vec<_>>(),
                expected,
            );
        }
        let mut components = FsPath::new(b"file/.").components();
        assert!(normal(components.next().unwrap(), b"file"));
        assert_eq!(components.next(), Some(Component::CurDir));
        assert_eq!(components.next(), None);
    }

    #[test]
    fn lone_root_yields_root_dir_in_both_directions() {
        let mut forward = FsPath::new(b"/").components();
        assert_eq!(forward.next(), Some(Component::RootDir));
        assert_eq!(forward.next(), None);

        let mut backward = FsPath::new(b"/").components();
        assert_eq!(backward.next_back(), Some(Component::RootDir));
        assert_eq!(backward.next_back(), None);
    }

    #[test]
    fn repeated_separators_after_root_are_skipped() {
        let mut components = FsPath::new(b"//a").components();
        assert_eq!(components.next(), Some(Component::RootDir));
        assert!(normal(components.next().unwrap(), b"a"));
        assert_eq!(components.next(), None);
    }

    #[test]
    fn popping_the_final_component_keeps_the_root_component() {
        // The resolver pops the final name with next_back and then walks the
        // rest in front order; the leading root component must survive that.
        let mut components = FsPath::new(b"/bin/sh").components();
        assert!(normal(components.next_back().unwrap(), b"sh"));
        assert_eq!(components.next(), Some(Component::RootDir));
        assert!(normal(components.next().unwrap(), b"bin"));
        assert_eq!(components.next(), None);
        assert_eq!(components.next_back(), None);
    }

    #[test]
    fn single_component_paths_keep_their_root_when_popped() {
        for path in [b"/a".as_slice(), b"/a/"] {
            let mut components = FsPath::new(path).components();
            assert!(normal(components.next_back().unwrap(), b"a"));
            assert_eq!(components.next_back(), Some(Component::RootDir));
            assert_eq!(components.next_back(), None);
            assert_eq!(components.next(), None);
            assert_eq!(
                FsPath::new(path).parent().map(FsPath::as_bytes),
                Some(b"/".as_slice()),
            );
        }
        let mut relative = FsPath::new(b"a").components();
        assert!(normal(relative.next_back().unwrap(), b"a"));
        assert_eq!(relative.next(), None);
        assert_eq!(relative.next_back(), None);
        assert_eq!(
            FsPath::new(b"a").parent().map(FsPath::as_bytes),
            Some(b"".as_slice()),
        );

        let mut dot = FsPath::new(b"/.").components();
        assert_eq!(dot.next_back(), Some(Component::CurDir));
        assert_eq!(dot.next_back(), Some(Component::RootDir));
        assert_eq!(dot.next_back(), None);
    }

    #[test]
    fn single_component_absolute_path_supports_mixed_iteration() {
        let mut from_back = FsPath::new(b"/a").components();
        assert!(normal(from_back.next_back().unwrap(), b"a"));
        assert_eq!(from_back.next(), Some(Component::RootDir));
        assert_eq!(from_back.next_back(), None);
        assert_eq!(from_back.next(), None);

        let mut from_front = FsPath::new(b"/a").components();
        assert_eq!(from_front.next(), Some(Component::RootDir));
        assert!(normal(from_front.next_back().unwrap(), b"a"));
        assert_eq!(from_front.next(), None);
        assert_eq!(from_front.next_back(), None);
    }

    #[test]
    fn file_name_and_parent_ignore_the_root_component() {
        assert_eq!(
            FsPath::new(b"/bin/sh").file_name().map(FsName::as_bytes),
            Some(&b"sh"[..])
        );
        assert_eq!(
            FsPath::new(b"/bin/sh").parent().map(FsPath::as_bytes),
            Some(&b"/bin"[..])
        );
    }
}
