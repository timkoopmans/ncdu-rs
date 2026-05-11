//! JSON export of a [`Tree`] in ncdu v2 wire format.
//!
//! Ported from `json_export.zig`. Departs from upstream in being a
//! post-scan tree walker rather than a streaming sink — we have no scanner
//! yet. Output is intended to be byte-for-byte compatible with
//! `ncdu --export-json` (after normalizing the header's progname/version
//! /timestamp fields).
//!
//! Compression (zstd) is not yet implemented.

use std::io::{self, Write};

use crate::model::{EType, EntryId, NodeKind, Tree};

pub struct ExportOptions {
    pub extended: bool,
    pub program_name: String,
    pub program_version: String,
    /// Override the unix timestamp written into the header. `None` = wall clock.
    pub timestamp: Option<u64>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            extended: false,
            program_name: "ncdu-rs".to_string(),
            program_version: env!("CARGO_PKG_VERSION").to_string(),
            timestamp: None,
        }
    }
}

pub fn export_tree<W: Write>(
    tree: &Tree,
    root: EntryId,
    w: &mut W,
    opts: &ExportOptions,
) -> io::Result<()> {
    let ts = opts.timestamp.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });
    write!(
        w,
        "[1,2,{{\"progname\":\"{}\",\"progver\":\"{}\",\"timestamp\":{}}}",
        opts.program_name, opts.program_version, ts
    )?;

    if !root.is_none() {
        // `u64::MAX` sentinel for "no parent dev" — guaranteed not to match any
        // real `st_dev`, so root always emits its `dev` field.
        write_dir(tree, root, u64::MAX, w, opts)?;
    }

    w.write_all(b"]\n")
}

fn write_dir<W: Write>(
    tree: &Tree,
    id: EntryId,
    parent_dev_raw: u64,
    w: &mut W,
    opts: &ExportOptions,
) -> io::Result<()> {
    let node = tree.get(id);
    let Some(dir) = node.as_dir() else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "write_dir on non-dir"));
    };
    let own_dev_raw = tree.devices.get(dir.dev).unwrap_or(0);

    w.write_all(b",\n[")?;
    write_entry_object(tree, id, parent_dev_raw, w, opts)?;
    if dir.err {
        w.write_all(b",\"read_error\":true")?;
    }
    w.write_all(b"}")?;

    let mut child = dir.sub;
    while !child.is_none() {
        let cnode = tree.get(child);
        match &cnode.kind {
            NodeKind::Dir(_) => write_dir(tree, child, own_dev_raw, w, opts)?,
            _ => {
                w.write_all(b",\n")?;
                write_entry_object(tree, child, own_dev_raw, w, opts)?;
                w.write_all(b"}")?;
            }
        }
        child = cnode.common.next;
    }

    w.write_all(b"]")
}

/// Writes the opening of an entry object: `{"name":"..","asize":N,...`
/// — without the closing brace. Caller appends extra fields then `}`.
fn write_entry_object<W: Write>(
    tree: &Tree,
    id: EntryId,
    parent_dev_raw: u64,
    w: &mut W,
    opts: &ExportOptions,
) -> io::Result<()> {
    let node = tree.get(id);
    let common = &node.common;

    // Handle "special" entries (pattern/otherfs/kernfs/err) the way upstream does.
    match common.etype {
        EType::Pattern | EType::OtherFs | EType::KernFs | EType::Err => {
            w.write_all(b"{\"name\":\"")?;
            write_escaped(w, common.name.as_bytes())?;
            let tag = match common.etype {
                EType::Err => "\",\"read_error\":true",
                EType::OtherFs => "\",\"excluded\":\"otherfs\"",
                EType::KernFs => "\",\"excluded\":\"kernfs\"",
                EType::Pattern => "\",\"excluded\":\"pattern\"",
                _ => unreachable!(),
            };
            return w.write_all(tag.as_bytes());
        }
        _ => {}
    }

    w.write_all(b"{\"name\":\"")?;
    write_escaped(w, common.name.as_bytes())?;
    w.write_all(b"\"")?;

    if common.size > 0 {
        write!(w, ",\"asize\":{}", common.size)?;
    }
    if common.blocks > 0 {
        write!(w, ",\"dsize\":{}", common.blocks.saturating_mul(512))?;
    }

    match &node.kind {
        NodeKind::Dir(d) => {
            let dev_real = tree.devices.get(d.dev).unwrap_or(0);
            if dev_real != parent_dev_raw {
                write!(w, ",\"dev\":{}", dev_real)?;
            }
        }
        NodeKind::Link(l) => {
            write!(w, ",\"ino\":{},\"hlnkc\":true,\"nlink\":{}", l.ino, l.nlink)?;
        }
        NodeKind::File => {
            if common.etype == EType::NonReg {
                w.write_all(b",\"notreg\":true")?;
            }
        }
    }

    if opts.extended {
        if let Some(ext) = &common.ext {
            if let Some(uid) = ext.uid {
                write!(w, ",\"uid\":{}", uid)?;
            }
            if let Some(gid) = ext.gid {
                write!(w, ",\"gid\":{}", gid)?;
            }
            if let Some(mode) = ext.mode {
                write!(w, ",\"mode\":{}", mode)?;
            }
            if let Some(mtime) = ext.mtime {
                write!(w, ",\"mtime\":{}", mtime)?;
            }
        }
    }
    Ok(())
}

fn write_escaped<W: Write>(w: &mut W, s: &[u8]) -> io::Result<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for &b in s {
        if b >= 0x20 && b != b'"' && b != b'\\' && b != 0x7F {
            w.write_all(&[b])?;
        } else {
            match b {
                b'\n' => w.write_all(b"\\n")?,
                b'\r' => w.write_all(b"\\r")?,
                0x08 => w.write_all(b"\\b")?,
                b'\t' => w.write_all(b"\\t")?,
                0x0C => w.write_all(b"\\f")?,
                b'\\' => w.write_all(b"\\\\")?,
                b'"' => w.write_all(b"\\\"")?,
                _ => {
                    w.write_all(b"\\u00")?;
                    w.write_all(&[HEX[(b >> 4) as usize], HEX[(b & 0xf) as usize]])?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DirData, EType, EntryCommon, EntryId, LinkData, Node, NodeKind, Tree};

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
        let dir = tree.get_mut(parent).as_dir_mut().unwrap();
        let old_head = dir.sub;
        dir.sub = child;
        dir.items += 1;
        tree.get_mut(child).common.next = old_head;
    }

    #[test]
    fn empty_root_dir() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/tmp", 1);
        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(
            s,
            "[1,2,{\"progname\":\"ncdu\",\"progver\":\"2.9.2\",\"timestamp\":1700000000},\n[{\"name\":\"/tmp\",\"dev\":1}]]\n"
        );
    }

    #[test]
    fn root_with_one_file() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/tmp", 1);
        let file = tree.create(EType::Reg, "a.txt");
        {
            let c = &mut tree.get_mut(file).common;
            c.size = 100;
            c.blocks = 1;
        }
        add_child(&mut tree, root, file);

        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#"{"name":"a.txt","asize":100,"dsize":512}"#), "got: {s}");
    }

    #[test]
    fn escapes_special_chars_in_name() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "weird\nname\"quote\\bs", 1);
        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains(r#""name":"weird\nname\"quote\\bs""#), "got: {s}");
    }

    #[test]
    fn link_entry_emits_hlnkc() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/", 1);
        let link = tree.create(EType::Link, "hard");
        {
            let n: &mut Node = tree.get_mut(link);
            n.common.size = 50;
            n.common.blocks = 1;
            if let NodeKind::Link(l) = &mut n.kind {
                l.ino = 42;
                l.nlink = 3;
            }
        }
        add_child(&mut tree, root, link);

        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains(r#""ino":42,"hlnkc":true,"nlink":3"#),
            "got: {s}"
        );
    }

    #[test]
    fn nested_dir_emits_array() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/", 1);
        let sub = tree.create(EType::Dir, "subdir");
        tree.get_mut(sub).as_dir_mut().unwrap().dev = tree.devices.get_id(1);
        add_child(&mut tree, root, sub);

        let f = tree.create(EType::Reg, "inside.txt");
        tree.get_mut(f).common.size = 7;
        add_child(&mut tree, sub, f);

        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        // Sub-dir is an array nested inside the root array; dev omitted because same as parent.
        assert!(s.contains("[{\"name\":\"subdir\"}"), "got: {s}");
        assert!(
            s.contains("{\"name\":\"inside.txt\",\"asize\":7}"),
            "got: {s}"
        );
        assert!(!s.contains("\"subdir\",\"dev\""), "dev should be elided: {s}");
    }

    #[test]
    fn excluded_otherfs_marker() {
        let mut tree = Tree::new();
        let root = make_root(&mut tree, "/", 1);
        let ex = tree.create(EType::OtherFs, "mount");
        add_child(&mut tree, root, ex);
        let mut out = Vec::new();
        export_tree(&tree, root, &mut out, &fixed_opts()).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains(r#"{"name":"mount","excluded":"otherfs"}"#),
            "got: {s}"
        );
    }

    // Suppress unused-import warning for Node/DirData/etc in some builds.
    #[allow(dead_code)]
    fn _unused(_: &DirData, _: &EntryCommon, _: &LinkData) {}
}
