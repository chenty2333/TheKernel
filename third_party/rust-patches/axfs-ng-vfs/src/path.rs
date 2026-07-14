use alloc::{borrow::ToOwned, string::String, sync::Arc};
use core::{borrow::Borrow, fmt, ops::Deref};

use crate::{VfsError, VfsResult};

pub const DOT: &str = ".";
pub const DOTDOT: &str = "..";

pub const MAX_NAME_LEN: usize = 255;

pub(crate) fn verify_entry_name(name: &str) -> VfsResult<()> {
    if name == DOT || name == DOTDOT {
        return Err(VfsError::InvalidInput);
    }
    if name.len() > MAX_NAME_LEN {
        return Err(VfsError::NameTooLong);
    }
    Ok(())
}

/// A single component of a [`Path`].
///
/// This corresponds to [`std::path::Component`].
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Component<'a> {
    RootDir,
    CurDir,
    ParentDir,
    Normal(&'a str),
}

impl<'a> Component<'a> {
    pub fn as_str(&self) -> &'a str {
        match self {
            Component::RootDir => "/",
            Component::CurDir => ".",
            Component::ParentDir => "..",
            Component::Normal(s) => s,
        }
    }
}

/// The final syntactic component of a pathname before any normalization.
///
/// Namespace-removal callers need to distinguish `name`, `.`, `..`, and the
/// root itself. In particular, treating `dir/.` as though its final component
/// were `dir` can turn a rejected operation into deletion of the wrong entry.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum FinalComponentKind<'a> {
    /// A named directory entry.
    Normal(&'a str),
    /// The current-directory component (`.`).
    Dot,
    /// The parent-directory component (`..`).
    DotDot,
    /// A pathname made entirely from root separators.
    Root,
}

/// Borrowed classification of one pathname's final syntactic component.
///
/// `requires_directory` preserves directory intent which would otherwise be
/// lost when trailing separators are skipped. It is true for `name/` and for
/// the intrinsically directory-valued `.`, `..`, and root components. The type
/// carries no ABI-specific error policy.
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

    /// Returns the unnormalized final-component kind.
    pub const fn kind(self) -> FinalComponentKind<'a> {
        self.kind
    }

    /// Returns whether the pathname syntax requires a directory at the end.
    pub const fn requires_directory(self) -> bool {
        self.requires_directory
    }
}

/// An iterator over the [`Component`]s of a [`Path`].
///
/// This corresponds to [`std::path::Components`].
#[doc(hidden)]
pub struct Components<'a> {
    path: &'a str,
    at_start: bool,
}

impl<'a> Components<'a> {
    pub fn as_path(&self) -> &'a Path {
        Path::new(self.path)
    }

    fn parse_forward(&mut self, comp: &'a str) -> Option<Component<'a>> {
        let comp = match comp {
            "" => {
                if self.at_start {
                    Some(Component::RootDir)
                } else {
                    None
                }
            }
            "." => {
                if self.at_start {
                    Some(Component::CurDir)
                } else {
                    None
                }
            }
            ".." => Some(Component::ParentDir),
            _ => Some(Component::Normal(comp)),
        };
        self.at_start = false;
        comp
    }

    fn parse_backward(&mut self, comp: &'a str, no_rest: bool) -> Option<Component<'a>> {
        match comp {
            "" => {
                if self.at_start && no_rest {
                    Some(Component::RootDir)
                } else {
                    None
                }
            }
            "." => {
                if self.at_start && no_rest {
                    Some(Component::CurDir)
                } else {
                    None
                }
            }
            ".." => Some(Component::ParentDir),
            _ => Some(Component::Normal(comp)),
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
            let (comp, rest) = match self.path.find('/') {
                Some(index) => (&self.path[..index], &self.path[index + 1..]),
                None => (self.path, ""),
            };
            self.path = rest;
            if let Some(comp) = self.parse_forward(comp) {
                return Some(comp);
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
            let (comp, rest) = match self.path.rfind('/') {
                Some(index) => (
                    &self.path[index + 1..],
                    &self.path[..(index + 1).min(self.path.len() - 1)],
                ),
                None => (self.path, ""),
            };
            self.path = rest;
            if let Some(comp) = self.parse_backward(comp, rest.is_empty()) {
                return Some(comp);
            }
        }
    }
}

/// A slice of path (akin to [`str`]).
///
/// Different from [`std::path::Path`], this type is always
/// UTF-8 encoded.
#[derive(Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Path {
    inner: str,
}

impl Path {
    pub fn new<S: AsRef<str> + ?Sized>(s: &S) -> &Path {
        unsafe { &*(s.as_ref() as *const str as *const Path) }
    }

    pub fn as_str(&self) -> &str {
        &self.inner
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.inner.as_bytes()
    }

    /// Produces an iterator over the [`Components`] of the path.
    pub fn components(&self) -> Components<'_> {
        Components {
            path: &self.inner,
            at_start: true,
        }
    }

    /// Returns the final component of the `Path`, if there is one.
    pub fn file_name(&self) -> Option<&str> {
        self.components().next_back().and_then(|p| match p {
            Component::Normal(p) => Some(p),
            _ => None,
        })
    }

    /// Splits off the final syntactic component without normalizing it.
    ///
    /// The returned parent is a borrowed prefix of this pathname. Resolving
    /// that prefix walks every true parent component: for example, `a/b/.`
    /// yields parent `a/b/` plus [`FinalComponentKind::Dot`], while `file/`
    /// yields an empty parent plus `Normal("file")` with directory intent.
    /// Empty paths have no final component.
    pub fn split_final_component(&self) -> Option<(&Path, FinalComponent<'_>)> {
        let raw = self.as_str();
        if raw.is_empty() {
            return None;
        }

        let without_trailing_slashes = raw.trim_end_matches('/');
        if without_trailing_slashes.is_empty() {
            return Some((
                Path::new(raw),
                FinalComponent::new(FinalComponentKind::Root, true),
            ));
        }

        let final_start = without_trailing_slashes
            .rfind('/')
            .map_or(0, |separator| separator + 1);
        let final_name = &without_trailing_slashes[final_start..];
        let kind = match final_name {
            DOT => FinalComponentKind::Dot,
            DOTDOT => FinalComponentKind::DotDot,
            name => FinalComponentKind::Normal(name),
        };
        let requires_directory = without_trailing_slashes.len() != raw.len()
            || !matches!(kind, FinalComponentKind::Normal(_));

        Some((
            Path::new(&raw[..final_start]),
            FinalComponent::new(kind, requires_directory),
        ))
    }

    /// Creates an owned [`PathBuf`] with path adjoined to `self`.
    pub fn join(&self, other: impl AsRef<Path>) -> PathBuf {
        let mut path = self.to_owned();
        path.push(other);
        path
    }

    /// Returns the `Path` without its final component, if there is one.
    pub fn parent(&self) -> Option<&Path> {
        let mut comps = self.components();
        let comp = comps.next_back();
        comp.and_then(move |p| match p {
            Component::Normal(_) | Component::CurDir | Component::ParentDir => {
                Some(comps.as_path())
            }
            _ => None,
        })
    }

    /// Returns `true` if the `Path` is absolute, i.e., if it is independent of
    /// the current directory.
    pub fn is_absolute(&self) -> bool {
        self.inner.starts_with('/')
    }

    /// Normalizes a path without performing I/O.
    pub fn normalize(&self) -> Option<PathBuf> {
        let mut ret = PathBuf::new();
        for component in self.components() {
            match component {
                Component::RootDir => {
                    ret.push("/");
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if !ret.pop() {
                        return None;
                    }
                }
                Component::Normal(c) => {
                    ret.push(c);
                }
            }
        }
        Some(ret)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

impl<'a> From<&'a str> for &'a Path {
    fn from(value: &'a str) -> Self {
        Path::new(value)
    }
}

impl ToOwned for Path {
    type Owned = PathBuf;

    fn to_owned(&self) -> Self::Owned {
        PathBuf {
            inner: self.inner.to_owned(),
        }
    }
}

impl From<&Path> for Arc<Path> {
    #[inline]
    fn from(v: &Path) -> Arc<Path> {
        let arc = Arc::<str>::from(&v.inner);
        unsafe { Arc::from_raw(Arc::into_raw(arc) as *const Path) }
    }
}

impl AsRef<str> for Path {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.inner[..]
    }
}

impl AsRef<Path> for Path {
    #[inline]
    fn as_ref(&self) -> &Path {
        self
    }
}

macro_rules! impl_as_ref {
    ($($t:ty),+) => {
        $(impl AsRef<Path> for $t {
            fn as_ref(&self) -> &Path {
                Path::new(self)
            }
        })+
    };
}

impl_as_ref!(str, String);

/// An owned, mutable [`Path`] (akin to [`String`]).
///
/// Different from [`std::path::PathBuf`], this type is always
/// UTF-8 encoded.
#[derive(Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct PathBuf {
    inner: String,
}

impl PathBuf {
    pub const fn new() -> Self {
        Self {
            inner: String::new(),
        }
    }

    pub fn pop(&mut self) -> bool {
        match self.parent().map(|p| p.as_str().len()) {
            Some(len) => {
                self.inner.truncate(len);
                true
            }
            None => false,
        }
    }

    pub fn push(&mut self, path: impl AsRef<Path>) {
        self._push(path.as_ref());
    }

    fn _push(&mut self, path: &Path) {
        if path.as_str().is_empty() {
            return;
        }
        if path.is_absolute() {
            self.inner.clear();
        } else if !self.inner.ends_with('/') {
            self.inner.push('/');
        }
        self.inner += path.as_str();
    }
}

fn build_absolute_path_with<T, F, R>(
    components_leaf_to_root: &[T],
    component_name: F,
    reserve: R,
) -> VfsResult<PathBuf>
where
    F: Fn(&T) -> &str,
    R: FnOnce(&mut String, usize) -> VfsResult<()>,
{
    let mut capacity = 1usize;
    for component in components_leaf_to_root.iter().rev() {
        let name = component_name(component);
        if name.is_empty() {
            continue;
        }
        capacity = capacity
            .checked_add(usize::from(capacity != 1))
            .and_then(|capacity| capacity.checked_add(name.len()))
            .ok_or(VfsError::NoMemory)?;
    }

    let mut path = String::new();
    reserve(&mut path, capacity)?;
    path.push('/');
    for component in components_leaf_to_root.iter().rev() {
        let name = component_name(component);
        if name.is_empty() {
            continue;
        }
        if path.len() != 1 {
            path.push('/');
        }
        path.push_str(name);
    }
    debug_assert!(path.len() <= capacity);
    Ok(PathBuf::from(path))
}

/// Builds an absolute path from components collected from leaf to root.
///
/// The caller may retain lightweight entry handles instead of cloning every
/// component name. Both capacity arithmetic and the only output allocation
/// are fallible, so a userspace-triggered path snapshot reports `NoMemory`
/// instead of reaching the kernel allocation-error handler.
pub(crate) fn try_build_absolute_path<T>(
    components_leaf_to_root: &[T],
    component_name: impl Fn(&T) -> &str,
) -> VfsResult<PathBuf> {
    build_absolute_path_with(components_leaf_to_root, component_name, |path, capacity| {
        path.try_reserve_exact(capacity)
            .map_err(|_| VfsError::NoMemory)
    })
}

impl<T: AsRef<Path>> FromIterator<T> for PathBuf {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut path = PathBuf::new();
        for item in iter {
            path.push(item);
        }
        path
    }
}

impl Borrow<Path> for PathBuf {
    fn borrow(&self) -> &Path {
        self
    }
}

impl Deref for PathBuf {
    type Target = Path;

    #[inline]
    fn deref(&self) -> &Path {
        Path::new(&self.inner)
    }
}

impl AsRef<str> for PathBuf {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.inner[..]
    }
}

impl AsRef<Path> for PathBuf {
    #[inline]
    fn as_ref(&self) -> &Path {
        self
    }
}

impl From<String> for PathBuf {
    fn from(value: String) -> Self {
        Self { inner: value }
    }
}

impl From<&str> for PathBuf {
    fn from(value: &str) -> Self {
        Self {
            inner: value.to_owned(),
        }
    }
}

impl fmt::Display for PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

#[cfg(test)]
mod test {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn fallible_absolute_builder_preserves_order_and_reserve_error() {
        let components = ["leaf", "parent", ""];
        assert_eq!(
            try_build_absolute_path(&components, |component| *component)
                .unwrap()
                .as_str(),
            "/parent/leaf"
        );
        assert_eq!(
            build_absolute_path_with(
                &components,
                |component| *component,
                |_, _| Err(VfsError::NoMemory),
            ),
            Err(VfsError::NoMemory)
        );
    }

    #[test]
    fn test_back_components() {
        for path in ["../fds/", "./fs", "fs", "../", "..", ".", "./."] {
            let path = Path::new(path);
            let forward: Vec<_> = path.components().collect();
            let mut backward: Vec<_> = path.components().rev().collect();
            backward.reverse();
            assert_eq!(forward, backward);
        }
    }

    #[test]
    fn test_file_name() {
        assert_eq!(Some("c"), Path::new("../a/b/c").file_name());
        assert_eq!(Some("b"), Path::new("a/b/.").file_name());
        assert_eq!(None, Path::new("a/..").file_name());
    }

    fn assert_final_component(
        raw: &str,
        expected_parent: &str,
        expected_kind: FinalComponentKind<'_>,
        requires_directory: bool,
    ) {
        let (parent, final_component) = Path::new(raw).split_final_component().unwrap();
        assert_eq!(parent.as_str(), expected_parent);
        assert_eq!(final_component.kind(), expected_kind);
        assert_eq!(final_component.requires_directory(), requires_directory);
    }

    #[test]
    fn final_component_classification_preserves_destructive_syntax() {
        assert_eq!(Path::new("").split_final_component(), None);
        assert_final_component("file", "", FinalComponentKind::Normal("file"), false);
        assert_final_component("file/", "", FinalComponentKind::Normal("file"), true);
        assert_final_component(
            "dir/file///",
            "dir/",
            FinalComponentKind::Normal("file"),
            true,
        );
        assert_final_component(".", "", FinalComponentKind::Dot, true);
        assert_final_component("..", "", FinalComponentKind::DotDot, true);
        assert_final_component("/", "/", FinalComponentKind::Root, true);
        assert_final_component("///", "///", FinalComponentKind::Root, true);
        assert_final_component("a/b/.", "a/b/", FinalComponentKind::Dot, true);
        assert_final_component("file/.", "file/", FinalComponentKind::Dot, true);
        assert_final_component("a/b/..", "a/b/", FinalComponentKind::DotDot, true);
    }
}
