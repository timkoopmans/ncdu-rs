//! JSON import of an ncdu v2 dump into a [`Tree`].
//!
//! Ported from `json_import.zig`. Departs from upstream by using
//! `serde_json::Value` rather than a custom byte-level parser. Consequence:
//! filenames must be valid UTF-8. Real ncdu dumps with non-UTF-8 paths are
//! not yet supported — that needs a custom parser, deferred to a follow-up.
//!
//! Sibling ordering matches the source dump (children linked in scan order).

use std::io::Read;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use crate::model::{EType, EntryId, Ext, NodeKind, Tree};

pub fn import_tree<R: Read>(rd: R) -> Result<Tree> {
    let v: Value = serde_json::from_reader(rd)?;
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("expected top-level array"))?;
    if arr.len() < 4 {
        bail!("expected [major, minor, metadata, root, ...] — got {} elements", arr.len());
    }
    let major = arr[0]
        .as_u64()
        .ok_or_else(|| anyhow!("major version must be a number"))?;
    if major != 1 {
        bail!("incompatible major format version: {major}");
    }
    // arr[1] = minor, ignored per upstream.
    // arr[2] = metadata, ignored.

    let mut tree = Tree::new();
    let root_id = parse_item(&arr[3], &mut tree, u64::MAX)?;
    tree.root = root_id;
    Ok(tree)
}

fn parse_item(v: &Value, tree: &mut Tree, parent_dev: u64) -> Result<EntryId> {
    let (is_dir, obj, children): (_, &Map<String, Value>, _) = match v {
        Value::Array(arr) => {
            if arr.is_empty() {
                bail!("empty array — directory entry requires at least an info object");
            }
            let obj = arr[0]
                .as_object()
                .ok_or_else(|| anyhow!("first array element must be an object"))?;
            (true, obj, Some(&arr[1..]))
        }
        Value::Object(obj) => (false, obj, None),
        _ => bail!("expected object or array, got {v}"),
    };

    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing or non-string \"name\" field"))?
        .to_owned();
    let size = obj.get("asize").and_then(Value::as_u64).unwrap_or(0);
    let dsize = obj.get("dsize").and_then(Value::as_u64).unwrap_or(0);
    let blocks = dsize >> 9;
    let dev = obj.get("dev").and_then(Value::as_u64).unwrap_or(parent_dev);
    let ino = obj.get("ino").and_then(Value::as_u64);
    let nlink = obj.get("nlink").and_then(Value::as_u64).unwrap_or(0) as u32;
    let hlnkc = obj.get("hlnkc").and_then(Value::as_bool).unwrap_or(false);
    let notreg = obj.get("notreg").and_then(Value::as_bool).unwrap_or(false);
    let read_error = obj.get("read_error").and_then(Value::as_bool).unwrap_or(false);
    let excluded = obj.get("excluded").and_then(Value::as_str);

    let etype = if is_dir {
        EType::Dir
    } else if let Some(ex) = excluded {
        // "frmlnk" → pattern per upstream, kept for compatibility.
        match ex {
            "otherfs" | "othfs" => EType::OtherFs,
            "kernfs" => EType::KernFs,
            _ => EType::Pattern,
        }
    } else if read_error {
        EType::Err
    } else if hlnkc || (nlink > 1 && ino.is_some()) {
        EType::Link
    } else if notreg {
        EType::NonReg
    } else {
        EType::Reg
    };

    let id = tree.create(etype, name);
    {
        let c = &mut tree.get_mut(id).common;
        c.size = size;
        c.blocks = blocks;
        let mut ext = Ext::default();
        if let Some(u) = obj.get("uid").and_then(Value::as_u64) {
            ext.uid = Some(u as u32);
        }
        if let Some(g) = obj.get("gid").and_then(Value::as_u64) {
            ext.gid = Some(g as u32);
        }
        if let Some(m) = obj.get("mode").and_then(Value::as_u64) {
            ext.mode = Some(m as u16);
        }
        if let Some(t) = obj.get("mtime").and_then(Value::as_u64) {
            ext.mtime = Some(t);
        }
        if !ext.is_empty() {
            c.ext = Some(ext);
        }
    }

    match &mut tree.get_mut(id).kind {
        NodeKind::Dir(d) => {
            // parent set by caller
            d.err = read_error;
        }
        NodeKind::Link(l) => {
            l.ino = ino.unwrap_or(0);
            l.nlink = nlink;
        }
        NodeKind::File => {}
    }

    // dev_id assignment for dirs uses parent's dev when not overridden — matches
    // upstream where Stat.dev defaults to parent_dev unless explicitly present.
    if let NodeKind::Dir(_) = &tree.get(id).kind {
        let dev_id = tree.devices.get_id(dev);
        if let Some(d) = tree.get_mut(id).as_dir_mut() {
            d.dev = dev_id;
        }
    }

    // Recurse into children, preserving source order via tail-append.
    if let Some(kids) = children {
        let mut child_ids: Vec<EntryId> = Vec::with_capacity(kids.len());
        for child_val in kids {
            let cid = parse_item(child_val, tree, dev)?;
            match &mut tree.get_mut(cid).kind {
                NodeKind::Dir(d) => d.parent = id,
                NodeKind::Link(l) => l.parent = id,
                NodeKind::File => {}
            }
            child_ids.push(cid);
        }
        if let Some(&first) = child_ids.first() {
            let count = child_ids.len() as u32;
            if let Some(d) = tree.get_mut(id).as_dir_mut() {
                d.sub = first;
                // Immediate-child count only. Full descendant total is the
                // job of a stats pass (not yet ported from upstream).
                d.items = count;
            }
            for window in child_ids.windows(2) {
                tree.get_mut(window[0]).common.next = window[1];
            }
        }
    }

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json_export::{export_tree, ExportOptions};
    use crate::model::{EType, NodeKind, Tree};

    fn fixed_opts() -> ExportOptions {
        ExportOptions {
            extended: false,
            program_name: "ncdu".to_string(),
            program_version: "2.9.2".to_string(),
            timestamp: Some(1_700_000_000),
        }
    }

    fn make_root(tree: &mut Tree, name: &str, dev: u64) -> EntryId {
        let dev_id = tree.devices.get_id(dev);
        let id = tree.create(EType::Dir, name);
        tree.get_mut(id).as_dir_mut().unwrap().dev = dev_id;
        tree.root = id;
        id
    }

    fn add_child(tree: &mut Tree, parent: EntryId, child: EntryId) {
        // Append, so resulting export order matches insertion order.
        let mut cur = tree.get(parent).as_dir().unwrap().sub;
        if cur.is_none() {
            tree.get_mut(parent).as_dir_mut().unwrap().sub = child;
        } else {
            while !tree.get(cur).common.next.is_none() {
                cur = tree.get(cur).common.next;
            }
            tree.get_mut(cur).common.next = child;
        }
        tree.get_mut(parent).as_dir_mut().unwrap().items += 1;
        match &mut tree.get_mut(child).kind {
            NodeKind::Dir(d) => d.parent = parent,
            NodeKind::Link(l) => l.parent = parent,
            NodeKind::File => {}
        }
    }

    #[test]
    fn round_trip_empty_root() {
        let mut tree = Tree::new();
        make_root(&mut tree, "/tmp", 7);
        let mut out = Vec::new();
        export_tree(&tree, tree.root, &mut out, &fixed_opts()).unwrap();
        let exported = String::from_utf8(out).unwrap();

        let imported = import_tree(exported.as_bytes()).unwrap();
        let mut out2 = Vec::new();
        export_tree(&imported, imported.root, &mut out2, &fixed_opts()).unwrap();
        assert_eq!(exported, String::from_utf8(out2).unwrap());
    }

    #[test]
    fn round_trip_mixed_tree() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/data", 42);

        let f = tree.create(EType::Reg, "file.txt");
        tree.get_mut(f).common.size = 1234;
        tree.get_mut(f).common.blocks = 4;
        add_child(&mut tree, root, f);

        let sub = tree.create(EType::Dir, "sub");
        tree.get_mut(sub).as_dir_mut().unwrap().dev = tree.devices.get_id(42);
        add_child(&mut tree, root, sub);

        let g = tree.create(EType::Reg, "inner");
        tree.get_mut(g).common.size = 99;
        add_child(&mut tree, sub, g);

        let link = tree.create(EType::Link, "hard");
        tree.get_mut(link).common.size = 50;
        tree.get_mut(link).common.blocks = 1;
        if let NodeKind::Link(l) = &mut tree.get_mut(link).kind {
            l.ino = 314;
            l.nlink = 2;
        }
        add_child(&mut tree, root, link);

        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let exported = String::from_utf8(out).unwrap();

        let imported = import_tree(exported.as_bytes()).unwrap();
        let mut out2 = Vec::new();
        export_tree(&imported, imported.root, &mut out2, &fixed_opts()).unwrap();
        assert_eq!(
            exported,
            String::from_utf8(out2).unwrap(),
            "round trip differs"
        );
    }

    #[test]
    fn round_trip_with_extended_attrs() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/", 1);
        let f = tree.create(EType::Reg, "x");
        {
            let c = &mut tree.get_mut(f).common;
            c.size = 10;
            c.blocks = 1;
            c.ext = Some(Ext {
                uid: Some(1000),
                gid: Some(1000),
                mode: Some(0o644),
                mtime: Some(1_600_000_000),
            });
        }
        add_child(&mut tree, root, f);

        let opts = ExportOptions {
            extended: true,
            ..fixed_opts()
        };
        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &opts).unwrap();
        let exported = String::from_utf8(out).unwrap();
        assert!(exported.contains("\"uid\":1000"), "{exported}");
        assert!(exported.contains("\"mode\":420"), "{exported}");

        let imported = import_tree(exported.as_bytes()).unwrap();
        let mut out2 = Vec::new();
        export_tree(&imported, imported.root, &mut out2, &opts).unwrap();
        assert_eq!(exported, String::from_utf8(out2).unwrap());
    }

    #[test]
    fn rejects_wrong_major_version() {
        let bad = br#"[2,0,{},[{"name":"x"}]]"#;
        let err = import_tree(bad.as_slice()).unwrap_err();
        assert!(format!("{err}").contains("incompatible major"));
    }

    #[test]
    fn parses_excluded_markers() {
        let json = br#"[1,2,{},[{"name":"r","dev":1},{"name":"mnt","excluded":"otherfs"},{"name":"k","excluded":"kernfs"}]]"#;
        let tree = import_tree(json.as_slice()).unwrap();
        let root = tree.root;
        let mut child = tree.get(root).as_dir().unwrap().sub;
        let mut found = Vec::new();
        while !child.is_none() {
            found.push(tree.get(child).common.etype);
            child = tree.get(child).common.next;
        }
        assert_eq!(found, vec![EType::OtherFs, EType::KernFs]);
    }
}
