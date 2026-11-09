//! Normalized remote paths.
//!
//! Every backend speaks a slightly different dialect (SMB wants `\dir\file`, WebDAV
//! wants percent-encoded URL segments, Google Drive has no paths at all). The VFS
//! therefore works on exactly one representation and each backend converts on the
//! edge: absolute, `/`-separated, no trailing slash, no `.`/`..` components.

use std::fmt;

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RPath(String);

impl RPath {
    pub fn root() -> Self {
        RPath(String::from("/"))
    }

    /// Normalizes any user- or OS-supplied path. Accepts both separators because
    /// Windows mount layers hand us backslashes.
    pub fn new(s: &str) -> Self {
        let mut out: Vec<&str> = Vec::new();
        for part in s.split(['/', '\\']) {
            match part {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                p => out.push(p),
            }
        }
        if out.is_empty() {
            Self::root()
        } else {
            RPath(format!("/{}", out.join("/")))
        }
    }

    pub fn is_root(&self) -> bool {
        self.0.len() == 1
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn join(&self, name: &str) -> Self {
        if name.contains('/') || name.contains('\\') {
            return RPath::new(&format!("{}/{}", self.0, name));
        }
        if self.is_root() {
            RPath(format!("/{name}"))
        } else {
            RPath(format!("{}/{}", self.0, name))
        }
    }

    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        match self.0.rfind('/') {
            Some(0) => Some(Self::root()),
            Some(i) => Some(RPath(self.0[..i].to_string())),
            None => None,
        }
    }

    pub fn name(&self) -> &str {
        if self.is_root() {
            return "/";
        }
        match self.0.rfind('/') {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// `\dir\file`; the root becomes the empty string, which is what SMB2 CREATE
    /// expects when you want to open the share root itself.
    pub fn to_backslash(&self) -> String {
        if self.is_root() {
            String::new()
        } else {
            self.0[1..].replace('/', "\\")
        }
    }

    /// Path relative to the root without a leading separator (`dir/file`).
    pub fn relative(&self) -> &str {
        if self.is_root() {
            ""
        } else {
            &self.0[1..]
        }
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.relative().split('/').filter(|s| !s.is_empty())
    }

    pub fn is_ancestor_of(&self, other: &RPath) -> bool {
        if self.is_root() {
            return !other.is_root();
        }
        other.0.len() > self.0.len()
            && other.0.as_bytes()[self.0.len()] == b'/'
            && other.0.starts_with(&self.0)
    }

    /// Rewrites `self` when `from` is renamed to `to`. Used to keep the inode table
    /// consistent after a directory rename - forgetting this is the classic source of
    /// "file disappeared after rename" bugs in user-mode filesystems.
    pub fn reparent(&self, from: &RPath, to: &RPath) -> Option<Self> {
        if self == from {
            return Some(to.clone());
        }
        if !from.is_ancestor_of(self) {
            return None;
        }
        let rest = &self.0[from.0.len()..];
        Some(RPath::new(&format!("{}{}", to.0, rest)))
    }
}

impl fmt::Display for RPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RPath({})", self.0)
    }
}

impl From<&str> for RPath {
    fn from(s: &str) -> Self {
        RPath::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        assert_eq!(RPath::new("").as_str(), "/");
        assert_eq!(RPath::new("/").as_str(), "/");
        assert_eq!(RPath::new("a/b/").as_str(), "/a/b");
        assert_eq!(RPath::new("\\a\\b").as_str(), "/a/b");
        assert_eq!(RPath::new("/a/./b/../c").as_str(), "/a/c");
        assert_eq!(RPath::new("/../../x").as_str(), "/x");
    }

    #[test]
    fn parts() {
        let p = RPath::new("/a/b/c.txt");
        assert_eq!(p.name(), "c.txt");
        assert_eq!(p.parent().unwrap().as_str(), "/a/b");
        assert_eq!(p.to_backslash(), "a\\b\\c.txt");
        assert_eq!(RPath::root().to_backslash(), "");
        assert_eq!(RPath::root().name(), "/");
        assert!(RPath::root().parent().is_none());
        assert_eq!(p.components().collect::<Vec<_>>(), vec!["a", "b", "c.txt"]);
    }

    #[test]
    fn ancestry_and_reparent() {
        let dir = RPath::new("/a/b");
        let file = RPath::new("/a/b/c.txt");
        assert!(dir.is_ancestor_of(&file));
        assert!(!file.is_ancestor_of(&dir));
        assert!(!RPath::new("/a/bb").is_ancestor_of(&file));
        assert!(RPath::root().is_ancestor_of(&file));

        let to = RPath::new("/z");
        assert_eq!(file.reparent(&dir, &to).unwrap().as_str(), "/z/c.txt");
        assert_eq!(dir.reparent(&dir, &to).unwrap().as_str(), "/z");
        assert!(RPath::new("/other").reparent(&dir, &to).is_none());
    }
}
