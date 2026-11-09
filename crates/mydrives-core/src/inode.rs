//! Inode bookkeeping.
//!
//! FUSE and NFS address files by inode number, not by path, and they expect a
//! number to keep pointing at the same object across renames. The table is the
//! single owner of the path <-> inode mapping; `rename` rewrites a whole subtree
//! so that an open handle on `/a/b/c.txt` keeps working after `/a/b` is renamed.

use std::collections::HashMap;

use crate::path::RPath;
use crate::types::{FileKind, Ino, ROOT_INO};

#[derive(Clone, Debug)]
pub struct Node {
    pub path: RPath,
    pub kind: FileKind,
}

pub struct InodeTable {
    by_ino: HashMap<Ino, Node>,
    by_path: HashMap<String, Ino>,
    next: Ino,
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

impl InodeTable {
    pub fn new() -> Self {
        let mut by_ino = HashMap::new();
        let mut by_path = HashMap::new();
        by_ino.insert(
            ROOT_INO,
            Node {
                path: RPath::root(),
                kind: FileKind::Dir,
            },
        );
        by_path.insert("/".to_string(), ROOT_INO);
        InodeTable {
            by_ino,
            by_path,
            next: ROOT_INO + 1,
        }
    }

    pub fn len(&self) -> usize {
        self.by_ino.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty()
    }

    pub fn get(&self, ino: Ino) -> Option<&Node> {
        self.by_ino.get(&ino)
    }

    pub fn ino_of(&self, path: &RPath) -> Option<Ino> {
        self.by_path.get(path.as_str()).copied()
    }

    /// Returns the existing inode for `path` or allocates one.
    pub fn intern(&mut self, path: &RPath, kind: FileKind) -> Ino {
        if let Some(&ino) = self.by_path.get(path.as_str()) {
            if let Some(node) = self.by_ino.get_mut(&ino) {
                node.kind = kind;
            }
            return ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_ino.insert(
            ino,
            Node {
                path: path.clone(),
                kind,
            },
        );
        self.by_path.insert(path.as_str().to_string(), ino);
        ino
    }

    /// Drops `path` and everything below it (after unlink/rmdir).
    pub fn remove_tree(&mut self, path: &RPath) {
        let doomed: Vec<Ino> = self
            .by_ino
            .iter()
            .filter(|(_, n)| &n.path == path || path.is_ancestor_of(&n.path))
            .map(|(ino, _)| *ino)
            .collect();
        for ino in doomed {
            if let Some(node) = self.by_ino.remove(&ino) {
                self.by_path.remove(node.path.as_str());
            }
        }
    }

    /// Rewrites `from` and all descendants to live under `to`.
    pub fn rename(&mut self, from: &RPath, to: &RPath) {
        // The destination may already be interned (rename-over); it is about to be
        // replaced, so drop it first.
        self.remove_tree(to);
        let moved: Vec<(Ino, RPath)> = self
            .by_ino
            .iter()
            .filter_map(|(ino, n)| n.path.reparent(from, to).map(|p| (*ino, p)))
            .collect();
        for (ino, new_path) in moved {
            if let Some(node) = self.by_ino.get_mut(&ino) {
                self.by_path.remove(node.path.as_str());
                node.path = new_path.clone();
                self.by_path.insert(new_path.into_string(), ino);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_one() {
        let t = InodeTable::new();
        assert_eq!(t.ino_of(&RPath::root()), Some(ROOT_INO));
        assert_eq!(t.get(ROOT_INO).unwrap().kind, FileKind::Dir);
    }

    #[test]
    fn intern_is_stable() {
        let mut t = InodeTable::new();
        let p = RPath::new("/a/b");
        let a = t.intern(&p, FileKind::File);
        let b = t.intern(&p, FileKind::File);
        assert_eq!(a, b);
        assert_ne!(a, ROOT_INO);
    }

    #[test]
    fn rename_moves_subtree_and_keeps_inodes() {
        let mut t = InodeTable::new();
        let dir = RPath::new("/a");
        let file = RPath::new("/a/f.txt");
        let dir_ino = t.intern(&dir, FileKind::Dir);
        let file_ino = t.intern(&file, FileKind::File);

        t.rename(&dir, &RPath::new("/z"));

        assert_eq!(t.get(dir_ino).unwrap().path.as_str(), "/z");
        assert_eq!(t.get(file_ino).unwrap().path.as_str(), "/z/f.txt");
        assert_eq!(t.ino_of(&RPath::new("/z/f.txt")), Some(file_ino));
        assert_eq!(t.ino_of(&file), None);
    }

    #[test]
    fn remove_tree_drops_children() {
        let mut t = InodeTable::new();
        t.intern(&RPath::new("/a"), FileKind::Dir);
        t.intern(&RPath::new("/a/b"), FileKind::File);
        t.intern(&RPath::new("/ab"), FileKind::File);
        t.remove_tree(&RPath::new("/a"));
        assert!(t.ino_of(&RPath::new("/a")).is_none());
        assert!(t.ino_of(&RPath::new("/a/b")).is_none());
        assert!(t.ino_of(&RPath::new("/ab")).is_some());
    }
}
