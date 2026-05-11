//! Core data model for the scanned filesystem tree.
//!
//! Ported from ncdu v2 `model.zig`. Departs from upstream by using an
//! arena (`Vec<Node>`) with `u32` indices in place of raw pointers and the
//! custom packed `(Ext +) [Dir|Link|File] + name` layout. Memory footprint
//! is higher than upstream (~2x per node) but the JSON wire format is
//! unaffected and the code is safe.

use std::collections::{HashMap, HashSet};

/// Index into [`Tree::nodes`]. `EntryId::NONE` represents a null reference.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryId(pub u32);

impl EntryId {
    pub const NONE: EntryId = EntryId(u32::MAX);

    pub fn is_none(self) -> bool {
        self == EntryId::NONE
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl Default for EntryId {
    fn default() -> Self {
        EntryId::NONE
    }
}

/// Entry type tag. Numeric values match the ncdu binfmt export and must stay stable.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(i8)]
pub enum EType {
    Dir = 0,
    Reg = 1,
    NonReg = 2,
    Link = 3,
    Err = -1,
    Pattern = -2,
    OtherFs = -3,
    KernFs = -4,
}

impl EType {
    /// Reduces extended types back to the three storable kinds.
    pub fn base(self) -> EType {
        match self {
            EType::Dir | EType::Link => self,
            _ => EType::Reg,
        }
    }

    /// Whether this entry should be displayed as a directory in the browser.
    pub fn is_directory(self) -> bool {
        matches!(self, EType::Dir | EType::OtherFs | EType::KernFs)
    }
}

/// Compressed device-id. ncdu uses `u30` to free flag bits; we keep `u32`
/// for simplicity and saturate at `u32::MAX - 1` if exceeded.
pub type DevId = u32;

/// Optional extended metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ext {
    pub mtime: Option<u64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u16>,
}

impl Ext {
    pub fn is_empty(&self) -> bool {
        self.mtime.is_none() && self.uid.is_none() && self.gid.is_none() && self.mode.is_none()
    }
}

/// Fields shared by every node kind.
#[derive(Clone, Debug)]
pub struct EntryCommon {
    pub name: Box<str>,
    pub etype: EType,
    /// Apparent size in bytes.
    pub size: u64,
    /// On-disk size in 512-byte blocks (upstream packs this into 60 bits).
    pub blocks: u64,
    pub ext: Option<Ext>,
    /// Next sibling in parent's child list. `EntryId::NONE` = end.
    pub next: EntryId,
}

impl EntryCommon {
    fn new(name: impl Into<Box<str>>, etype: EType) -> Self {
        Self {
            name: name.into(),
            etype,
            size: 0,
            blocks: 0,
            ext: None,
            next: EntryId::NONE,
        }
    }
}

/// Directory-specific data.
#[derive(Clone, Debug, Default)]
pub struct DirData {
    /// First child in this directory. `EntryId::NONE` = empty.
    pub sub: EntryId,
    /// Parent directory. `EntryId::NONE` only valid for the root.
    pub parent: EntryId,
    /// Bytes in shared hardlinks that still have refs outside this dir.
    pub shared_size: u64,
    /// 512-byte blocks in shared hardlinks (see `shared_size`).
    pub shared_blocks: u64,
    /// Total item count including all descendants.
    pub items: u32,
    pub dev: DevId,
    /// Scan error encountered on this directory itself.
    pub err: bool,
    /// Scan error encountered in some descendant.
    pub suberr: bool,
}

/// Hardlinked-file-specific data. Files with `nlink > 1` become `Link` nodes
/// and participate in the global inode table.
#[derive(Clone, Debug)]
pub struct LinkData {
    pub parent: EntryId,
    /// Circular linked-list of every Link with the same (dev,ino).
    pub next_link: EntryId,
    pub prev_link: EntryId,
    pub ino: u64,
    /// Whether this inode has been counted toward parent dir sizes.
    pub counted: bool,
    /// Reported `nlink` from the filesystem. `0` = unknown (old JSON dumps).
    pub nlink: u32,
}

impl Default for LinkData {
    fn default() -> Self {
        Self {
            parent: EntryId::NONE,
            next_link: EntryId::NONE,
            prev_link: EntryId::NONE,
            ino: 0,
            counted: false,
            nlink: 0,
        }
    }
}

/// Discriminator over node kinds. `File` carries no extra data beyond `EntryCommon`.
#[derive(Clone, Debug)]
pub enum NodeKind {
    Dir(DirData),
    Link(LinkData),
    File,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub common: EntryCommon,
    pub kind: NodeKind,
}

impl Node {
    pub fn as_dir(&self) -> Option<&DirData> {
        if let NodeKind::Dir(d) = &self.kind { Some(d) } else { None }
    }

    pub fn as_dir_mut(&mut self) -> Option<&mut DirData> {
        if let NodeKind::Dir(d) = &mut self.kind { Some(d) } else { None }
    }

    pub fn as_link(&self) -> Option<&LinkData> {
        if let NodeKind::Link(l) = &self.kind { Some(l) } else { None }
    }

    pub fn as_link_mut(&mut self) -> Option<&mut LinkData> {
        if let NodeKind::Link(l) = &mut self.kind { Some(l) } else { None }
    }

    pub fn has_err(&self) -> bool {
        match &self.kind {
            NodeKind::Dir(d) => d.err || d.suberr,
            _ => self.common.etype == EType::Err,
        }
    }
}

/// Deduplicated device-id table. Real `st_dev` values are 64-bit but a scan
/// typically touches only a handful of devices, so we compress to a [`DevId`].
#[derive(Debug, Default)]
pub struct Devices {
    pub list: Vec<u64>,
    lookup: HashMap<u64, DevId>,
}

impl Devices {
    pub fn get_id(&mut self, dev: u64) -> DevId {
        if let Some(&id) = self.lookup.get(&dev) {
            return id;
        }
        let id = self.list.len() as DevId;
        self.list.push(dev);
        self.lookup.insert(dev, id);
        id
    }

    pub fn get(&self, id: DevId) -> Option<u64> {
        self.list.get(id as usize).copied()
    }
}

/// Inode bookkeeping for hardlink counting. Mirrors `model.zig#inodes`.
#[derive(Debug, Default)]
pub struct Inodes {
    /// `(dev, ino) -> arbitrary Link node in that inode's circular list`.
    pub map: HashMap<(DevId, u64), EntryId>,
    /// Links not yet folded into parent-dir totals.
    pub uncounted: HashSet<EntryId>,
    /// Once `uncounted` grows past `map.len() / 8`, we drop it and walk `map`
    /// directly in `add_all_stats`.
    pub uncounted_full: bool,
}

/// Owns every node in the scanned tree.
#[derive(Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: EntryId,
    pub devices: Devices,
    pub inodes: Inodes,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocates a new node and returns its id.
    pub fn create(&mut self, etype: EType, name: impl Into<Box<str>>) -> EntryId {
        let common = EntryCommon::new(name, etype);
        let kind = match etype {
            EType::Dir => NodeKind::Dir(DirData::default()),
            EType::Link => NodeKind::Link(LinkData::default()),
            _ => NodeKind::File,
        };
        let id = EntryId(self.nodes.len() as u32);
        self.nodes.push(Node { common, kind });
        id
    }

    pub fn get(&self, id: EntryId) -> &Node {
        &self.nodes[id.index()]
    }

    pub fn get_mut(&mut self, id: EntryId) -> &mut Node {
        &mut self.nodes[id.index()]
    }

    /// Walks parent links and writes the full path into `out`. If `with_root`
    /// is false, the root's own name is omitted.
    pub fn fmt_path(&self, id: EntryId, with_root: bool, out: &mut String) {
        let mut components: Vec<&str> = Vec::new();
        let mut cur = id;
        while !cur.is_none() {
            let node = self.get(cur);
            let parent = match &node.kind {
                NodeKind::Dir(d) => d.parent,
                NodeKind::Link(l) => l.parent,
                NodeKind::File => EntryId::NONE,
            };
            if with_root || !parent.is_none() {
                components.push(&node.common.name);
            }
            cur = parent;
        }
        for (i, comp) in components.iter().rev().enumerate() {
            if i != 0 && !out.ends_with('/') {
                out.push('/');
            }
            out.push_str(comp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_file_entry() {
        let mut tree = Tree::new();
        let id = tree.create(EType::Reg, "hello");
        let node = tree.get(id);
        assert_eq!(node.common.etype, EType::Reg);
        assert!(node.common.ext.is_none());
        assert_eq!(&*node.common.name, "hello");
    }

    #[test]
    fn create_dir_entry() {
        let mut tree = Tree::new();
        let id = tree.create(EType::Dir, "root");
        assert!(tree.get(id).as_dir().is_some());
    }

    #[test]
    fn dev_id_dedup() {
        let mut d = Devices::default();
        assert_eq!(d.get_id(42), 0);
        assert_eq!(d.get_id(99), 1);
        assert_eq!(d.get_id(42), 0);
        assert_eq!(d.list, vec![42, 99]);
    }

    #[test]
    fn etype_classification() {
        assert!(EType::Dir.is_directory());
        assert!(EType::OtherFs.is_directory());
        assert!(EType::KernFs.is_directory());
        assert!(!EType::Reg.is_directory());
        assert!(!EType::Link.is_directory());
        assert_eq!(EType::Pattern.base(), EType::Reg);
        assert_eq!(EType::Dir.base(), EType::Dir);
    }
}
