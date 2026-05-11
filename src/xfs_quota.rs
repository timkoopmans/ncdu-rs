//! XFS project-quota fast-path.
//!
//! When a directory has an XFS project quota applied, the kernel already
//! tracks total bytes used in O(1). Calling out to `xfs_quota report` is
//! orders of magnitude faster than walking millions of files. This module
//! detects whether the fast-path is usable for a given path and, if so,
//! returns the byte total.
//!
//! Fast-path requirements (all must hold):
//! - Path is on an XFS filesystem (`statfs.f_type == XFS_SUPER_MAGIC`)
//! - `xfs_quota` is installed and on `$PATH` (xfsprogs)
//! - A project ID is mapped to this directory in `/etc/projects` and the
//!   project quota is active on the mount
//! - Caller has permission to run `xfs_quota -x` (typically root)
//!
//! Any failure (not XFS / no quota / parse error / `xfs_quota` missing)
//! is silently reported as `None` so the caller can fall back to a walk.
//!
//! ## Why this matters
//!
//! For a 50M-file XFS volume, `du -s` or any walker takes minutes. The
//! kernel's project quota counters return the same number in microseconds.
//! Building this fast-path into ncdu-rs is the project's value-add over
//! upstream ncdu, which does not consult XFS quota.

#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

const XFS_SUPER_MAGIC: i64 = 0x58465342;

/// Returns `true` when the path lives on an XFS filesystem.
pub fn is_xfs(path: &Path) -> io::Result<bool> {
    let cpath = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf.f_type as i64 == XFS_SUPER_MAGIC)
}

/// Resolves a path to its XFS project ID by scanning `/etc/projects`.
/// Returns `None` if no entry matches.
pub fn project_id_for(path: &Path) -> io::Result<Option<u32>> {
    let canonical = fs::canonicalize(path)?;
    let target = canonical.to_string_lossy();
    let projects = match fs::read_to_string("/etc/projects") {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for line in projects.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, ':');
        let id = match parts.next().and_then(|s| s.trim().parse::<u32>().ok()) {
            Some(v) => v,
            None => continue,
        };
        let p = match parts.next() {
            Some(p) => p.trim(),
            None => continue,
        };
        if p == target {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Runs `xfs_quota -x -c 'report -np <project_id>' <mount>` and parses the
/// used-bytes column. Returns `None` if `xfs_quota` is unavailable, the
/// project has no quota row, or parsing fails.
pub fn quota_usage_bytes(project_id: u32, mount: &Path) -> Option<u64> {
    let out = Command::new("xfs_quota")
        .arg("-x")
        .arg("-c")
        .arg(format!("report -np {project_id}"))
        .arg(mount)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_xfs_quota_report(&String::from_utf8_lossy(&out.stdout), project_id)
}

/// Picks the `Used` column for the matching `#<id>` row from
/// `xfs_quota report -np` output.
///
/// Sample input:
/// ```text
/// Project quota on /mnt/data (/dev/sda1)
///                                Blocks
/// Project ID       Used       Soft       Hard    Warn/Grace
/// ---------- --------------------------------------------------
/// #42            1234560          0          0     00 [--------]
/// ```
fn parse_xfs_quota_report(text: &str, project_id: u32) -> Option<u64> {
    let needle = format!("#{project_id}");
    for line in text.lines() {
        let trimmed = line.trim_start();
        let mut iter = trimmed.split_whitespace();
        let first = iter.next()?;
        if first != needle {
            continue;
        }
        // Second column is "Used" in 1-KiB units per xfs_quota default report.
        let used_kib: u64 = iter.next()?.parse().ok()?;
        return Some(used_kib.saturating_mul(1024));
    }
    None
}

/// One-shot fast-path: check XFS + project mapping + run `xfs_quota`. Returns
/// `Ok(Some(bytes))` on success, `Ok(None)` if fast-path unusable, `Err`
/// only for unexpected I/O failures during detection.
pub fn try_quota_total(path: &Path) -> io::Result<Option<u64>> {
    if !is_xfs(path)? {
        return Ok(None);
    }
    let pid = match project_id_for(path)? {
        Some(p) => p,
        None => return Ok(None),
    };
    // Use the path itself as the mount argument; xfs_quota accepts any path
    // on the filesystem.
    Ok(quota_usage_bytes(pid, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_xfs_quota_report_picks_matching_row() {
        let sample = "\
Project quota on /mnt/data (/dev/sda1)
                               Blocks
Project ID       Used       Soft       Hard    Warn/Grace
---------- --------------------------------------------------
#41               1024          0          0     00 [--------]
#42            1234560          0          0     00 [--------]
#43                  0          0          0     00 [--------]
";
        assert_eq!(parse_xfs_quota_report(sample, 42), Some(1234560 * 1024));
        assert_eq!(parse_xfs_quota_report(sample, 41), Some(1024 * 1024));
        assert_eq!(parse_xfs_quota_report(sample, 99), None);
    }

    #[test]
    fn parse_handles_empty_input() {
        assert_eq!(parse_xfs_quota_report("", 1), None);
        assert_eq!(parse_xfs_quota_report("garbage no rows", 1), None);
    }

    #[test]
    fn is_xfs_returns_false_for_tmp() {
        // /tmp on macOS/most-linux dev boxes won't be XFS. Just confirm
        // the call returns without panicking. (On a real XFS dev box this
        // would assert true, but we can't depend on that.)
        let _ = is_xfs(Path::new("/tmp"));
    }
}
