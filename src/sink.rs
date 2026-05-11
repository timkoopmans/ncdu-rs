//! Generic sink API for scan results.
//!
//! Ported (single-threaded subset) from `sink.zig` + `mem_sink.zig`. v0 keeps
//! only the in-memory sink (`MemSink`) and runs synchronously. JSON-streaming
//! sink and multi-threaded operation are deferred.
//!
//! Dir lifetime is explicit via `finalize(&mut sink)` rather than upstream's
//! atomic refcount + `unref`. Single-threaded scope makes this straightforward.

use crate::model::{EType, EntryId, Ext, NodeKind, Tree};

/// Stat fields captured by the scanner, in the same shape upstream `sink.Stat`
/// passes around. `etype` selects which `NodeKind` the sink will build.
#[derive(Clone, Debug, Default)]
pub struct Stat {
    pub etype: EType,
    /// Apparent size in bytes.
    pub size: u64,
    /// 512-byte blocks (kernel reports this directly via `MetadataExt::blocks`).
    pub blocks: u64,
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
    pub ext: Ext,
}

impl Default for EType {
    fn default() -> Self {
        EType::Reg
    }
}

/// In-memory sink. Owns the [`Tree`] being built and exposes [`MemSinkDir`]
/// handles for accumulating children.
pub struct MemSink {
    tree: Tree,
    /// Aggregate per-dir blocks/size into ancestor totals. Disable for refresh
    /// scenarios where totals are recomputed later. Defaults to `true`.
    pub aggregate_stats: bool,
}

impl MemSink {
    pub fn new() -> Self {
        Self {
            tree: Tree::new(),
            aggregate_stats: true,
        }
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    pub fn into_tree(self) -> Tree {
        self.tree
    }

    /// Allocates the root directory. Must be called exactly once before any
    /// per-dir operations.
    pub fn create_root(&mut self, name: &str, stat: &Stat) -> MemSinkDir {
        let dev_id = self.tree.devices.get_id(stat.dev);
        let id = self.tree.create(EType::Dir, name);
        {
            let node = self.tree.get_mut(id);
            node.common.size = stat.size;
            node.common.blocks = stat.blocks;
            if !stat.ext.is_empty() {
                node.common.ext = Some(stat.ext.clone());
            }
            if let NodeKind::Dir(d) = &mut node.kind {
                d.dev = dev_id;
            }
        }
        self.tree.root = id;
        MemSinkDir {
            id,
            own_blocks: stat.blocks,
            own_size: stat.size,
            sub_blocks: 0,
            sub_size: 0,
            sub_items: 0,
            suberr: false,
        }
    }
}

impl Default for MemSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to an in-progress directory. The scanner accumulates child entries
/// here and calls [`MemSinkDir::finalize`] to flush totals into the parent.
pub struct MemSinkDir {
    /// Underlying tree node id.
    id: EntryId,
    /// Blocks reported for the directory entry itself (so we can subtract).
    own_blocks: u64,
    own_size: u64,
    /// Totals contributed by descendants — added to parent at `finalize`.
    sub_blocks: u64,
    sub_size: u64,
    sub_items: u32,
    suberr: bool,
}

impl MemSinkDir {
    pub fn id(&self) -> EntryId {
        self.id
    }

    pub fn set_read_error(&mut self, sink: &mut MemSink) {
        if let Some(d) = sink.tree.get_mut(self.id).as_dir_mut() {
            d.err = true;
        }
    }

    /// Adds a special marker entry: error / excluded-by-pattern / otherfs / kernfs.
    pub fn add_special(&mut self, sink: &mut MemSink, name: &str, sp: EType) {
        debug_assert!(matches!(
            sp,
            EType::Err | EType::Pattern | EType::OtherFs | EType::KernFs
        ));
        let cid = sink.tree.create(sp, name);
        self.link_child(&mut sink.tree, cid);
        if sink.aggregate_stats {
            self.sub_items += 1;
            if sp == EType::Err {
                self.suberr = true;
                if let Some(d) = sink.tree.get_mut(self.id).as_dir_mut() {
                    d.suberr = true;
                }
            }
        }
    }

    /// Adds a regular / non-regular / link file entry.
    pub fn add_stat(&mut self, sink: &mut MemSink, name: &str, stat: &Stat) -> EntryId {
        debug_assert!(stat.etype != EType::Dir);
        let cid = sink.tree.create(stat.etype, name);
        {
            let node = sink.tree.get_mut(cid);
            node.common.size = stat.size;
            node.common.blocks = stat.blocks;
            if !stat.ext.is_empty() {
                node.common.ext = Some(stat.ext.clone());
            }
            if let NodeKind::Link(l) = &mut node.kind {
                l.ino = stat.ino;
                l.nlink = stat.nlink;
                l.parent = self.id;
            }
        }
        self.link_child(&mut sink.tree, cid);
        if sink.aggregate_stats {
            self.sub_items += 1;
            // Upstream skips link-blocks here; the dedup happens in
            // `inodes.setStats` which we do not run in v0. Counting them once
            // gives wrong-but-bounded totals until the stats pass lands.
            if stat.etype != EType::Link {
                self.sub_blocks = self.sub_blocks.saturating_add(stat.blocks);
                self.sub_size = self.sub_size.saturating_add(stat.size);
            }
        }
        cid
    }

    /// Creates a subdirectory and returns a handle for filling it. Caller must
    /// invoke `finalize` on the returned handle before this one finalizes.
    pub fn add_dir(&mut self, sink: &mut MemSink, name: &str, stat: &Stat) -> MemSinkDir {
        debug_assert!(stat.etype == EType::Dir);
        let dev_id = sink.tree.devices.get_id(stat.dev);
        let cid = sink.tree.create(EType::Dir, name);
        {
            let node = sink.tree.get_mut(cid);
            node.common.size = stat.size;
            node.common.blocks = stat.blocks;
            if !stat.ext.is_empty() {
                node.common.ext = Some(stat.ext.clone());
            }
            if let NodeKind::Dir(d) = &mut node.kind {
                d.dev = dev_id;
                d.parent = self.id;
            }
        }
        self.link_child(&mut sink.tree, cid);
        if sink.aggregate_stats {
            self.sub_items += 1;
            self.sub_blocks = self.sub_blocks.saturating_add(stat.blocks);
            self.sub_size = self.sub_size.saturating_add(stat.size);
        }
        MemSinkDir {
            id: cid,
            own_blocks: stat.blocks,
            own_size: stat.size,
            sub_blocks: 0,
            sub_size: 0,
            sub_items: 0,
            suberr: false,
        }
    }

    /// Flushes accumulated descendant totals into the directory node, and
    /// (if `parent` provided) into the parent's sub-totals.
    pub fn finalize(self, sink: &mut MemSink, parent: Option<&mut MemSinkDir>) {
        if !sink.aggregate_stats {
            return;
        }
        if let Some(d) = sink.tree.get_mut(self.id).as_dir_mut() {
            d.items = d.items.saturating_add(self.sub_items);
        }
        {
            let node = sink.tree.get_mut(self.id);
            node.common.blocks = node.common.blocks.saturating_add(self.sub_blocks);
            node.common.size = node.common.size.saturating_add(self.sub_size);
        }
        if let Some(p) = parent {
            // Push descendants minus the dir entry's own footprint (which was
            // already added to p.sub_blocks via add_dir's bookkeeping).
            let total_blocks = self.sub_blocks;
            let total_size = self.sub_size;
            p.sub_blocks = p.sub_blocks.saturating_add(total_blocks);
            p.sub_size = p.sub_size.saturating_add(total_size);
            p.sub_items = p.sub_items.saturating_add(self.sub_items);
            if self.suberr {
                p.suberr = true;
                if let Some(pd) = sink.tree.get_mut(p.id).as_dir_mut() {
                    pd.suberr = true;
                }
            }
        }
        let _ = (self.own_blocks, self.own_size); // present for symmetry with upstream; unused in v0.
    }

    fn link_child(&mut self, tree: &mut Tree, child: EntryId) {
        let dir = tree.get_mut(self.id).as_dir_mut().expect("link_child on non-dir");
        let head = dir.sub;
        if head.is_none() {
            dir.sub = child;
        } else {
            // Walk to tail to preserve scan order. O(n) per insert is fine
            // for v0; switch to tracking a tail pointer when perf matters.
            let mut cur = head;
            while !tree.get(cur).common.next.is_none() {
                cur = tree.get(cur).common.next;
            }
            tree.get_mut(cur).common.next = child;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EType;

    fn stat_reg(size: u64, blocks: u64) -> Stat {
        Stat {
            etype: EType::Reg,
            size,
            blocks,
            dev: 1,
            ..Stat::default()
        }
    }

    fn stat_dir(dev: u64) -> Stat {
        Stat {
            etype: EType::Dir,
            size: 4096,
            blocks: 8,
            dev,
            ..Stat::default()
        }
    }

    #[test]
    fn builds_flat_tree() {
        let mut s = MemSink::new();
        let mut root = s.create_root("/tmp", &stat_dir(1));
        root.add_stat(&mut s, "a", &stat_reg(100, 1));
        root.add_stat(&mut s, "b", &stat_reg(200, 1));
        root.finalize(&mut s, None);

        let tree = s.into_tree();
        let r = tree.get(tree.root);
        assert_eq!(&*r.common.name, "/tmp");
        let dir = r.as_dir().unwrap();
        assert_eq!(dir.items, 2);
        // Walk children in order.
        let mut names = Vec::new();
        let mut cur = dir.sub;
        while !cur.is_none() {
            names.push(tree.get(cur).common.name.clone().into_string());
            cur = tree.get(cur).common.next;
        }
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn nested_dir_aggregates_blocks() {
        let mut s = MemSink::new();
        let mut root = s.create_root("/", &stat_dir(1));
        let mut sub = root.add_dir(&mut s, "sub", &stat_dir(1));
        sub.add_stat(&mut s, "x", &stat_reg(10, 2));
        sub.add_stat(&mut s, "y", &stat_reg(20, 4));
        sub.finalize(&mut s, Some(&mut root));
        root.finalize(&mut s, None);

        let tree = s.into_tree();
        // root.blocks = own (8) + sub_dir.own (8) + sub_dir.children (6) = 22
        let root_blocks = tree.get(tree.root).common.blocks;
        assert_eq!(root_blocks, 8 + 8 + 6, "got {root_blocks}");
        let root_dir = tree.get(tree.root).as_dir().unwrap();
        assert_eq!(root_dir.items, 3);
    }

    #[test]
    fn special_entries_propagate_suberr() {
        let mut s = MemSink::new();
        let mut root = s.create_root("/", &stat_dir(1));
        let mut sub = root.add_dir(&mut s, "bad", &stat_dir(1));
        sub.add_special(&mut s, "broken", EType::Err);
        sub.finalize(&mut s, Some(&mut root));
        root.finalize(&mut s, None);

        let tree = s.into_tree();
        assert!(tree.get(tree.root).as_dir().unwrap().suberr);
    }

    #[test]
    fn add_dir_subtree_can_round_trip_via_json() {
        use crate::json_export::{export_tree, ExportOptions};
        use crate::json_import::import_tree;

        let mut s = MemSink::new();
        let mut root = s.create_root("/", &stat_dir(1));
        root.add_stat(&mut s, "f", &stat_reg(5, 1));
        let mut sub = root.add_dir(&mut s, "d", &stat_dir(1));
        sub.add_stat(&mut s, "g", &stat_reg(6, 1));
        sub.finalize(&mut s, Some(&mut root));
        root.finalize(&mut s, None);

        let tree = s.into_tree();
        let opts = ExportOptions {
            program_name: "ncdu".to_string(),
            program_version: "2.9.2".to_string(),
            timestamp: Some(0),
            extended: false,
        };
        let mut out = Vec::new();
        export_tree(&tree, tree.root, &mut out, &opts).unwrap();
        let imported = import_tree(out.as_slice()).unwrap();
        let mut out2 = Vec::new();
        export_tree(&imported, imported.root, &mut out2, &opts).unwrap();
        assert_eq!(out, out2);
    }
}
