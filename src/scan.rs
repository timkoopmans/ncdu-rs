//! Filesystem scanner.
//!
//! Ported (single-threaded subset) from `scan.zig`. v0 walks one directory
//! at a time with `std::fs::read_dir` and `MetadataExt`. Deferred to
//! follow-ups:
//! - Parallel work-stealing across N threads (mirror upstream's State queue)
//! - Exclude patterns (`exclude.zig` port)
//! - `same_fs`, `follow_symlinks`, `exclude_kernfs`, `exclude_caches`
//! - `CACHEDIR.TAG` detection
//!
//! Symlinks are not followed; matches upstream's `no_follow: true` default.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::model::{EType, Ext, Tree};
use crate::sink::{MemSink, MemSinkDir, Stat};

pub struct ScanOptions {
    /// Capture mtime/uid/gid/mode into `Ext` for every entry.
    pub extended: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { extended: false }
    }
}

pub fn scan(path: &Path, opts: &ScanOptions) -> io::Result<Tree> {
    // For the root we follow symlinks (upstream's `statAt(.., follow=true)`).
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("scan target is not a directory: {}", path.display()),
        ));
    }
    let root_stat = stat_from_metadata(&metadata, opts.extended);
    let name = path.to_string_lossy().into_owned();

    let mut sink = MemSink::new();
    let mut root = sink.create_root(&name, &root_stat);
    scan_dir(path, &mut sink, &mut root, opts);
    root.finalize(&mut sink, None);
    Ok(sink.into_tree())
}

fn scan_dir(path: &Path, sink: &mut MemSink, dir: &mut MemSinkDir, opts: &ScanOptions) {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => {
            dir.set_read_error(sink);
            return;
        }
    };

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name_os = entry.file_name();
        let name = match name_os.to_str() {
            Some(n) => n.to_owned(),
            None => {
                // Non-UTF-8 filenames are not supported in v0 (model uses Box<str>).
                // Skip with a placeholder error marker so the tree records the gap.
                dir.add_special(sink, &name_os.to_string_lossy(), EType::Err);
                continue;
            }
        };
        let child_path = entry.path();

        let metadata = match fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(_) => {
                dir.add_special(sink, &name, EType::Err);
                continue;
            }
        };

        let stat = stat_from_metadata(&metadata, opts.extended);

        if metadata.file_type().is_dir() {
            let mut sub = dir.add_dir(sink, &name, &stat);
            scan_dir(&child_path, sink, &mut sub, opts);
            sub.finalize(sink, Some(dir));
        } else {
            dir.add_stat(sink, &name, &stat);
        }
    }
}

fn stat_from_metadata(m: &fs::Metadata, extended: bool) -> Stat {
    let ft = m.file_type();
    let etype = if ft.is_dir() {
        EType::Dir
    } else if m.nlink() > 1 && ft.is_file() {
        EType::Link
    } else if !ft.is_file() {
        EType::NonReg
    } else {
        EType::Reg
    };
    let ext = if extended {
        Ext {
            mtime: Some(m.mtime().max(0) as u64),
            uid: Some(m.uid()),
            gid: Some(m.gid()),
            mode: Some(m.mode() as u16),
        }
    } else {
        Ext::default()
    };
    Stat {
        etype,
        size: m.size(),
        blocks: m.blocks(),
        dev: m.dev(),
        ino: m.ino(),
        nlink: m.nlink() as u32,
        ext,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use tempfile::tempdir;

    fn write_file(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn scans_empty_dir() {
        let td = tempdir().unwrap();
        let tree = scan(td.path(), &ScanOptions::default()).unwrap();
        let root = tree.get(tree.root);
        assert_eq!(root.as_dir().unwrap().items, 0);
    }

    #[test]
    fn scans_flat_files() {
        let td = tempdir().unwrap();
        write_file(&td.path().join("a"), b"hello");
        write_file(&td.path().join("b"), b"world!");
        let tree = scan(td.path(), &ScanOptions::default()).unwrap();
        let root = tree.get(tree.root);
        assert_eq!(root.as_dir().unwrap().items, 2);
        let mut names = Vec::new();
        let mut cur = root.as_dir().unwrap().sub;
        while !cur.is_none() {
            names.push(tree.get(cur).common.name.to_string());
            cur = tree.get(cur).common.next;
        }
        names.sort();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn scans_nested_dirs() {
        let td = tempdir().unwrap();
        fs::create_dir(td.path().join("sub")).unwrap();
        write_file(&td.path().join("sub").join("inner"), b"x");
        write_file(&td.path().join("top"), b"yyy");
        let tree = scan(td.path(), &ScanOptions::default()).unwrap();

        let root = tree.get(tree.root);
        let dir = root.as_dir().unwrap();
        assert_eq!(dir.items, 3); // top, sub, inner

        // Locate the "sub" child and verify its child "inner".
        let mut sub_id = None;
        let mut cur = dir.sub;
        while !cur.is_none() {
            if &*tree.get(cur).common.name == "sub" {
                sub_id = Some(cur);
                break;
            }
            cur = tree.get(cur).common.next;
        }
        let sub_id = sub_id.expect("sub dir not in tree");
        let sub_dir = tree.get(sub_id).as_dir().unwrap();
        assert_eq!(sub_dir.items, 1);
        let inner = tree.get(sub_dir.sub);
        assert_eq!(&*inner.common.name, "inner");
        assert_eq!(inner.common.size, 1);
    }

    #[test]
    fn scan_then_export_then_import_round_trip() {
        use crate::json_export::{export_tree, ExportOptions};
        use crate::json_import::import_tree;

        let td = tempdir().unwrap();
        fs::create_dir(td.path().join("d")).unwrap();
        write_file(&td.path().join("d").join("g"), b"hi");
        write_file(&td.path().join("f"), b"hello");

        let tree = scan(td.path(), &ScanOptions::default()).unwrap();

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

    #[test]
    fn rejects_non_directory_path() {
        let td = tempdir().unwrap();
        let f = td.path().join("not-a-dir");
        write_file(&f, b"x");
        let err = scan(&f, &ScanOptions::default()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotADirectory);
    }

    #[test]
    fn extended_attrs_populated_when_requested() {
        let td = tempdir().unwrap();
        write_file(&td.path().join("x"), b"hi");
        let tree = scan(td.path(), &ScanOptions { extended: true }).unwrap();
        let root = tree.get(tree.root);
        let child = tree.get(root.as_dir().unwrap().sub);
        let ext = child.common.ext.as_ref().expect("ext should be populated");
        assert!(ext.uid.is_some());
        assert!(ext.gid.is_some());
        assert!(ext.mode.is_some());
        assert!(ext.mtime.is_some());
    }
}
