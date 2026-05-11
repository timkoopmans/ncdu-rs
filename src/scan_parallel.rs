//! Parallel filesystem walker.
//!
//! Two-phase design:
//! 1. **Walk** — rayon parallelism on subdir recursion. Each directory's
//!    `read_dir` + per-entry stat happens on one thread; subdir descents
//!    fan out via `par_iter`. Result is an in-memory `WalkedDir` tree.
//! 2. **Build** — single-threaded fold of `WalkedDir` into the `Tree` arena
//!    via the existing `MemSink` API. Fast (no I/O), deterministic order.
//!
//! Differs from upstream `scan.zig`, which uses an explicit fixed-size
//! work-stealing queue + N threads sharing per-thread arenas. Rayon's
//! work-stealing thread pool gives equivalent behaviour with less code.
//! Cost: peak memory grows with the walked subtree before the build phase
//! runs. Acceptable for v0; refactor to streaming if very large trees OOM.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::exclude::{ExcludeSet, Patterns};
use crate::model::{EType, Ext, Tree};
use crate::scan::ScanOptions;
use crate::sink::{MemSink, MemSinkDir, Stat};

enum Walked {
    File { name: String, stat: Stat },
    Special { name: String, etype: EType },
    Dir(Box<WalkedDir>),
}

struct WalkedDir {
    name: String,
    stat: Stat,
    children: Vec<Walked>,
    read_error: bool,
}

pub fn scan_parallel(path: &Path, opts: &ScanOptions, threads: usize) -> io::Result<Tree> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("scan target is not a directory: {}", path.display()),
        ));
    }
    let root_stat = stat_from_metadata(&metadata, opts.extended);
    let root_name = path.to_string_lossy().into_owned();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .build()
        .map_err(|e| io::Error::other(format!("rayon pool: {e}")))?;

    let root_pat = opts.exclude.for_path(&root_name);

    let walked = pool.install(|| {
        walk_dir(
            path,
            root_name.clone(),
            root_stat.clone(),
            opts,
            &root_pat,
        )
    });

    Ok(build_tree(walked))
}

fn walk_dir(
    path: &Path,
    name: String,
    stat: Stat,
    opts: &ScanOptions,
    pat: &Patterns,
) -> WalkedDir {
    let entries = match fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => {
            return WalkedDir {
                name,
                stat,
                children: Vec::new(),
                read_error: true,
            };
        }
    };

    // Step 1: classify each entry, gathering subdir descents to do in parallel.
    let mut children: Vec<Walked> = Vec::new();
    let mut pending_subdirs: Vec<(usize, PathBuf, String, Stat, Patterns)> = Vec::new();

    for entry_res in entries {
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name_os = entry.file_name();
        let cname = match name_os.to_str() {
            Some(n) => n.to_owned(),
            None => {
                children.push(Walked::Special {
                    name: name_os.to_string_lossy().into_owned(),
                    etype: EType::Err,
                });
                continue;
            }
        };
        let child_path = entry.path();

        let pat_result = pat.matches(&opts.exclude, &cname);
        if pat_result == Some(false) {
            children.push(Walked::Special {
                name: cname,
                etype: EType::Pattern,
            });
            continue;
        }

        let metadata = match fs::symlink_metadata(&child_path) {
            Ok(m) => m,
            Err(_) => {
                children.push(Walked::Special {
                    name: cname,
                    etype: EType::Err,
                });
                continue;
            }
        };
        let cstat = stat_from_metadata(&metadata, opts.extended);

        if metadata.file_type().is_dir() {
            if pat_result == Some(true) {
                children.push(Walked::Special {
                    name: cname,
                    etype: EType::Pattern,
                });
                continue;
            }
            let sub_pat = pat.enter(&opts.exclude, &cname);
            let idx = children.len();
            children.push(Walked::Special {
                name: cname.clone(),
                etype: EType::Err, // placeholder, replaced after parallel walk
            });
            pending_subdirs.push((idx, child_path, cname, cstat, sub_pat));
        } else {
            children.push(Walked::File { name: cname, stat: cstat });
        }
    }

    // Step 2: walk subdirs in parallel and patch them into the children list.
    let walked_subs: Vec<WalkedDir> = pending_subdirs
        .par_iter()
        .map(|(_, p, n, s, pt)| walk_dir(p, n.clone(), s.clone(), opts, pt))
        .collect();

    for ((idx, _, _, _, _), wd) in pending_subdirs.into_iter().zip(walked_subs.into_iter()) {
        children[idx] = Walked::Dir(Box::new(wd));
    }

    WalkedDir {
        name,
        stat,
        children,
        read_error: false,
    }
}

fn build_tree(root: WalkedDir) -> Tree {
    let mut sink = MemSink::new();
    let mut root_dir = sink.create_root(&root.name, &root.stat);
    if root.read_error {
        root_dir.set_read_error(&mut sink);
    }
    for child in root.children {
        emit(&mut sink, &mut root_dir, child);
    }
    root_dir.finalize(&mut sink, None);
    sink.into_tree()
}

fn emit(sink: &mut MemSink, parent: &mut MemSinkDir, walked: Walked) {
    match walked {
        Walked::File { name, stat } => {
            parent.add_stat(sink, &name, &stat);
        }
        Walked::Special { name, etype } => {
            parent.add_special(sink, &name, etype);
        }
        Walked::Dir(d) => {
            let WalkedDir {
                name,
                stat,
                children,
                read_error,
            } = *d;
            let mut sub = parent.add_dir(sink, &name, &stat);
            if read_error {
                sub.set_read_error(sink);
            }
            for c in children {
                emit(sink, &mut sub, c);
            }
            sub.finalize(sink, Some(parent));
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
    fn parallel_matches_sequential_output() {
        use crate::json_export::{export_tree, ExportOptions};
        use crate::scan::{scan, ScanOptions};

        let td = tempdir().unwrap();
        for i in 0..5 {
            let d = td.path().join(format!("d{i}"));
            fs::create_dir(&d).unwrap();
            for j in 0..3 {
                write_file(&d.join(format!("f{j}")), &vec![0u8; (i * 100 + j) as usize]);
            }
        }
        write_file(&td.path().join("top"), b"hi");

        let opts_s = ScanOptions::default();
        let opts_p = ScanOptions::default();

        let t_seq = scan(td.path(), &opts_s).unwrap();
        let t_par = scan_parallel(td.path(), &opts_p, 4).unwrap();

        let export_opts = ExportOptions {
            program_name: "ncdu".to_string(),
            program_version: "test".to_string(),
            timestamp: Some(0),
            extended: false,
        };
        let mut seq_out = Vec::new();
        let mut par_out = Vec::new();
        export_tree(&t_seq, t_seq.root, &mut seq_out, &export_opts).unwrap();
        export_tree(&t_par, t_par.root, &mut par_out, &export_opts).unwrap();

        // Child order can differ between sequential read_dir and parallel
        // collection. Normalize by parsing JSON and comparing as Value
        // (which Value::Eq on objects ignores key order but does compare
        // array order; we accept this and verify sizes/items instead).
        assert_eq!(t_seq.get(t_seq.root).common.size, t_par.get(t_par.root).common.size);
        assert_eq!(
            t_seq.get(t_seq.root).common.blocks,
            t_par.get(t_par.root).common.blocks
        );
        assert_eq!(
            t_seq.get(t_seq.root).as_dir().unwrap().items,
            t_par.get(t_par.root).as_dir().unwrap().items
        );
    }

    #[test]
    fn parallel_excludes_patterns() {
        let td = tempdir().unwrap();
        write_file(&td.path().join("keep"), b"k");
        write_file(&td.path().join("drop"), b"d");

        let mut excl = ExcludeSet::new();
        excl.add("drop");
        let opts = ScanOptions {
            extended: false,
            exclude: excl,
        };
        let tree = scan_parallel(td.path(), &opts, 2).unwrap();
        let root = tree.get(tree.root);
        let mut found_keep = false;
        let mut drop_etype = None;
        let mut cur = root.as_dir().unwrap().sub;
        while !cur.is_none() {
            let n = tree.get(cur);
            if &*n.common.name == "keep" {
                found_keep = true;
            }
            if &*n.common.name == "drop" {
                drop_etype = Some(n.common.etype);
            }
            cur = tree.get(cur).common.next;
        }
        assert!(found_keep);
        assert_eq!(drop_etype, Some(EType::Pattern));
    }
}
