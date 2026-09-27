use std::fmt;

use serde::{Deserialize, Serialize};

const MAX_NAME: usize = 255;
const MAX_PATH: usize = 4096;

/// A validated path inside a space: UTF-8 names joined by `/`, without a
/// leading slash. The root is the empty path.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SpacePath(String);

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid path {0:?}")]
pub struct InvalidPath(pub String);

impl SpacePath {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The last name, or `""` for the root.
    pub fn name(&self) -> &str {
        self.0.rsplit_once('/').map_or(&self.0, |(_, name)| name)
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        let parent = self.0.rsplit_once('/').map_or("", |(parent, _)| parent);
        Some(Self(parent.to_owned()))
    }

    pub fn join(&self, name: &str) -> Result<Self, InvalidPath> {
        if self.is_root() {
            return Self::try_from(name);
        }
        Self::try_from(format!("{}/{name}", self.0))
    }

    /// True if `self` is `ancestor` or lies below it.
    pub fn is_within(&self, ancestor: &SpacePath) -> bool {
        ancestor.is_root()
            || self
                .0
                .strip_prefix(&ancestor.0)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    }

    /// Moves `self` from under `from` to under `to`, as a directory rename does.
    pub fn rebase(&self, from: &SpacePath, to: &SpacePath) -> Option<Self> {
        if !self.is_within(from) {
            return None;
        }
        let rest = &self.0[from.0.len()..];
        let rebased = match (to.is_root(), rest.strip_prefix('/')) {
            (true, Some(rest)) => rest.to_owned(),
            _ => format!("{}{rest}", to.0),
        };
        Self::try_from(rebased).ok()
    }
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && name != "." && name != ".." && !name.contains('\0')
}

impl TryFrom<String> for SpacePath {
    type Error = InvalidPath;

    fn try_from(path: String) -> Result<Self, InvalidPath> {
        let valid = path.is_empty() || (path.len() <= MAX_PATH && path.split('/').all(is_valid_name));
        if valid {
            Ok(Self(path))
        } else {
            Err(InvalidPath(path))
        }
    }
}

impl TryFrom<&str> for SpacePath {
    type Error = InvalidPath;

    fn try_from(path: &str) -> Result<Self, InvalidPath> {
        Self::try_from(path.to_owned())
    }
}

impl From<SpacePath> for String {
    fn from(path: SpacePath) -> Self {
        path.0
    }
}

impl fmt::Display for SpacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> SpacePath {
        SpacePath::try_from(s).unwrap()
    }

    #[test]
    fn rejects_traversal_and_empty_names() {
        for bad in ["/a", "a/", "a//b", ".", "a/..", "a/./b", "a\0b"] {
            assert!(SpacePath::try_from(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn parent_name_and_join() {
        let p = path("a/b/c");
        assert_eq!(p.name(), "c");
        assert_eq!(p.parent(), Some(path("a/b")));
        assert_eq!(path("a").parent(), Some(SpacePath::root()));
        assert_eq!(SpacePath::root().parent(), None);
        assert_eq!(SpacePath::root().join("x").unwrap(), path("x"));
        assert!(path("a").join("..").is_err());
    }

    #[test]
    fn within_respects_name_boundaries() {
        assert!(path("a/b").is_within(&path("a")));
        assert!(path("a").is_within(&path("a")));
        assert!(path("a").is_within(&SpacePath::root()));
        assert!(!path("ab").is_within(&path("a")));
    }

    #[test]
    fn rebase_moves_subtrees() {
        assert_eq!(path("a/b/c").rebase(&path("a/b"), &path("x")), Some(path("x/c")));
        assert_eq!(path("a/b").rebase(&path("a/b"), &path("x/y")), Some(path("x/y")));
        assert_eq!(
            path("a/b").rebase(&path("a"), &SpacePath::root()),
            Some(path("b"))
        );
        assert_eq!(path("ab").rebase(&path("a"), &path("x")), None);
    }

    #[test]
    fn serde_validates() {
        assert!(serde_json::from_str::<SpacePath>("\"a/../b\"").is_err());
        assert_eq!(serde_json::from_str::<SpacePath>("\"a/b\"").unwrap(), path("a/b"));
    }
}
