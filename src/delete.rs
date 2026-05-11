//! Filesystem delete operations with Tree pruning.
//!
//! Ported from `delete.zig`. v0 keeps only the core "delete entry + prune
//! tree + adjust ancestor totals" logic. Deferred:
//! - Confirm/error dialogs (live in `browser`)
//! - External `delete_command` invocation
//! - Refresh-on-stat-failure re-scan
//! - Hardlink dedup in stat adjustment (mirrors v0 scan limitation)

use std::fs;
use std::io;
use std::path::Path;

use crate::model::{EntryId, NodeKind, Tree};

/// Deletes `target` from disk and prunes it from `tree`. `parent` must be the
/// directory that owns `target`.
pub fn delete_entry(
    tree: &mut Tree,
    parent: EntryId,
    target: EntryId,
    root_on_disk: &Path,
) -> io::Result<()> {
    debug_assert!(!parent.is_none());
    debug_assert!(!target.is_none());

    // 1. Compute disk path.
    let rel = relative_path(tree, parent, target);
    let abs = root_on_disk.join(rel.trim_start_matches('/'));

    // 2. Remove from disk.
    let metadata = fs::symlink_metadata(&abs)?;
    if metadata.is_dir() {
        fs::remove_dir_all(&abs)?;
    } else {
        fs::remove_file(&abs)?;
    }

    // 3. Unlink target from parent's child chain.
    unlink_from_parent(tree, parent, target);

    // 4. Subtract target's footprint from every ancestor (parent included).
    let (sub_blocks, sub_size, sub_items) = subtree_totals(tree, target);
    let mut ancestor = parent;
    while !ancestor.is_none() {
        {
            let n = tree.get_mut(ancestor);
            n.common.blocks = n.common.blocks.saturating_sub(sub_blocks);
            n.common.size = n.common.size.saturating_sub(sub_size);
        }
        if let Some(d) = tree.get_mut(ancestor).as_dir_mut() {
            d.items = d.items.saturating_sub(sub_items);
        }
        ancestor = match &tree.get(ancestor).kind {
            NodeKind::Dir(d) => d.parent,
            _ => EntryId::NONE,
        };
    }

    Ok(())
}

/// Computes a path from the tree root to `target`, joined with `/`. The root's
/// own name is omitted (callers join against the on-disk root path). `parent`
/// is required because `File` nodes do not carry parent links.
pub fn relative_path(tree: &Tree, parent: EntryId, target: EntryId) -> String {
    let mut components: Vec<String> = vec![tree.get(target).common.name.to_string()];
    let mut cur = parent;
    while !cur.is_none() {
        let node = tree.get(cur);
        let next = match &node.kind {
            NodeKind::Dir(d) => d.parent,
            _ => break,
        };
        // Stop before pushing the tree root's own name.
        if next.is_none() {
            break;
        }
        components.push(node.common.name.to_string());
        cur = next;
    }
    let mut out = String::new();
    for comp in components.iter().rev() {
        out.push('/');
        out.push_str(comp);
    }
    out
}

fn unlink_from_parent(tree: &mut Tree, parent: EntryId, target: EntryId) {
    let head = tree.get(parent).as_dir().expect("parent must be dir").sub;
    if head == target {
        let next = tree.get(target).common.next;
        tree.get_mut(parent).as_dir_mut().unwrap().sub = next;
        return;
    }
    let mut cur = head;
    while !cur.is_none() {
        let next = tree.get(cur).common.next;
        if next == target {
            let after = tree.get(target).common.next;
            tree.get_mut(cur).common.next = after;
            return;
        }
        cur = next;
    }
}

/// Returns (blocks, size, items_including_self) for the subtree rooted at `id`.
fn subtree_totals(tree: &Tree, id: EntryId) -> (u64, u64, u32) {
    let node = tree.get(id);
    let mut blocks = node.common.blocks;
    let mut size = node.common.size;
    let mut items: u32 = 1;
    if let NodeKind::Dir(d) = &node.kind {
        // The dir node's own blocks/size already cover all descendants thanks
        // to the aggregation pass in MemSinkDir::finalize. Items are tracked
        // as immediate-child count in v0 — recurse for full descendant total.
        let mut stack = vec![d.sub];
        while let Some(mut cur) = stack.pop() {
            while !cur.is_none() {
                items = items.saturating_add(1);
                if let NodeKind::Dir(sub) = &tree.get(cur).kind {
                    stack.push(sub.sub);
                }
                cur = tree.get(cur).common.next;
            }
        }
        // Subtract the +1 we counted for the dir itself a second time above.
        items = items.saturating_sub(0); // (no-op, kept for clarity)
    }
    let _ = (&mut blocks, &mut size);
    (blocks, size, items)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, ScanOptions};
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn find_child<'t>(tree: &'t Tree, parent: EntryId, name: &str) -> EntryId {
        let mut cur = tree.get(parent).as_dir().unwrap().sub;
        while !cur.is_none() {
            if &*tree.get(cur).common.name == name {
                return cur;
            }
            cur = tree.get(cur).common.next;
        }
        EntryId::NONE
    }

    #[test]
    fn deletes_file_and_prunes_tree() {
        let td = tempdir().unwrap();
        write_file(&td.path().join("keep"), b"k");
        write_file(&td.path().join("drop"), b"dropped");

        let mut tree = scan(td.path(), &ScanOptions::default()).unwrap();
        let root = tree.root;
        let drop_id = find_child(&tree, root, "drop");
        assert!(!drop_id.is_none());

        delete_entry(&mut tree, root, drop_id, td.path()).unwrap();

        // File gone from disk.
        assert!(!td.path().join("drop").exists());
        assert!(td.path().join("keep").exists());

        // Pruned from tree.
        let mut found_drop = false;
        let mut cur = tree.get(root).as_dir().unwrap().sub;
        while !cur.is_none() {
            if &*tree.get(cur).common.name == "drop" {
                found_drop = true;
            }
            cur = tree.get(cur).common.next;
        }
        assert!(!found_drop);
        assert_eq!(tree.get(root).as_dir().unwrap().items, 1);
    }

    #[test]
    fn deletes_subdirectory_recursively() {
        let td = tempdir().unwrap();
        fs::create_dir(td.path().join("d")).unwrap();
        write_file(&td.path().join("d").join("inner"), b"x");
        fs::create_dir(td.path().join("d").join("deeper")).unwrap();
        write_file(&td.path().join("d").join("deeper").join("z"), b"yy");

        let mut tree = scan(td.path(), &ScanOptions::default()).unwrap();
        let root = tree.root;
        let d_id = find_child(&tree, root, "d");

        let root_items_before = tree.get(root).as_dir().unwrap().items;
        assert_eq!(root_items_before, 4); // d, inner, deeper, z

        delete_entry(&mut tree, root, d_id, td.path()).unwrap();
        assert!(!td.path().join("d").exists());

        let cur = tree.get(root).as_dir().unwrap().sub;
        assert!(cur.is_none(), "root should be empty after delete");
        assert_eq!(tree.get(root).as_dir().unwrap().items, 0);
    }

    #[test]
    fn delete_missing_path_returns_err() {
        let td = tempdir().unwrap();
        write_file(&td.path().join("gone"), b"x");
        let mut tree = scan(td.path(), &ScanOptions::default()).unwrap();
        let root = tree.root;
        let gone = find_child(&tree, root, "gone");

        // Remove behind ncdu's back.
        fs::remove_file(td.path().join("gone")).unwrap();

        let err = delete_entry(&mut tree, root, gone, td.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }
}
