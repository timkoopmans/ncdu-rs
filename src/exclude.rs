//! Exclude-pattern matcher.
//!
//! Ported from `exclude.zig`. Pattern semantics match upstream:
//! - `*`, `?`, `[abc]`, `[a-c]`, `[!abc]` via libc `fnmatch` (single-component only)
//! - Anchored patterns start with `/` and are matched against absolute paths
//! - Unanchored patterns can match at any path level (rsync semantics)
//! - Trailing `/` makes a pattern dir-only
//!
//! Differs from upstream: globals replaced with an explicit [`ExcludeSet`]
//! passed through the scanner. Multi-threaded scan stays safe because
//! `Patterns` clones are read-only after construction.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

/// One path component of a pattern. Multi-component patterns ("a/b/c") form
/// a singly-linked chain via `sub`.
#[derive(Debug)]
struct Pattern {
    pattern: CString,
    isdir: bool,
    isliteral: bool,
    sub: Option<Box<Pattern>>,
}

impl Pattern {
    fn is_literal(s: &[u8]) -> bool {
        !s.iter()
            .any(|&c| matches!(c, b'[' | b'*' | b'?' | b'\\'))
    }

    fn parse(pat: &str) -> Box<Pattern> {
        // Trim leading slashes only; trailing-slash conveys dir-only and stays.
        let trimmed = pat.trim_start_matches('/');
        let bytes = trimmed.as_bytes();

        // Build chain by splitting on '/'.
        let mut head: Option<Box<Pattern>> = None;
        let mut tail: *mut Pattern = std::ptr::null_mut();

        let mut rest = bytes;
        loop {
            if let Some(idx) = rest.iter().position(|&b| b == b'/') {
                let comp = &rest[..idx];
                rest = &rest[idx + 1..];

                let node = Box::new(Pattern {
                    pattern: CString::new(comp).unwrap_or_else(|_| CString::default()),
                    isdir: true,
                    isliteral: Self::is_literal(comp),
                    sub: None,
                });
                let raw = Box::into_raw(node);
                if head.is_none() {
                    // SAFETY: we just allocated this Box.
                    head = Some(unsafe { Box::from_raw(raw) });
                    tail = head.as_deref_mut().unwrap() as *mut Pattern;
                } else {
                    // SAFETY: tail is a live Box owned by head's chain.
                    unsafe {
                        (*tail).sub = Some(Box::from_raw(raw));
                        tail = (*tail).sub.as_deref_mut().unwrap() as *mut Pattern;
                    }
                }

                // Pattern ending in '/' (or "//") -> stop, trailing component
                // belongs to the parent as dir-only with no further sub.
                if rest.iter().all(|&b| b == b'/') {
                    return head.unwrap();
                }
            } else {
                // Final component — file-or-dir match (not dir-only).
                let node = Box::new(Pattern {
                    pattern: CString::new(rest).unwrap_or_else(|_| CString::default()),
                    isdir: false,
                    isliteral: Self::is_literal(rest),
                    sub: None,
                });
                if head.is_none() {
                    return node;
                }
                // SAFETY: tail valid.
                unsafe {
                    (*tail).sub = Some(node);
                }
                return head.unwrap();
            }
        }
    }
}

/// Set of patterns matched at one directory level. Two kinds:
/// - `leaf`: patterns whose match terminates here (file or dir to exclude)
/// - `branch`: patterns that must descend into a matching subdir before applying
#[derive(Default, Clone, Debug)]
pub struct Patterns {
    leaf: Vec<&'static Pattern>,
    branch: Vec<&'static Pattern>,
    isroot: bool,
}

impl Patterns {
    /// Match `name` against patterns at this level + unanchored patterns.
    /// Returns `None` if no match, `Some(false)` if file/dir must be excluded
    /// regardless of type, `Some(true)` if only dir version excluded.
    pub fn matches(&self, set: &ExcludeSet, name: &str) -> Option<bool> {
        let a = match_list(&self.leaf, name);
        if a == Some(false) {
            return Some(false);
        }
        let b = match_list(&set.root_unanchored.leaf, name);
        if b == Some(false) {
            return Some(false);
        }
        a.or(b)
    }

    /// Build the [`Patterns`] for a subdirectory `name` under this level.
    pub fn enter(&self, set: &ExcludeSet, name: &str) -> Patterns {
        let mut out = Patterns::default();
        enter_into(&self.branch, name, &mut out);
        enter_into(&set.root_unanchored.branch, name, &mut out);
        out
    }
}

fn match_list(list: &[&Pattern], name: &str) -> Option<bool> {
    let cname = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return None, // names with NUL — never match
    };
    let mut ret: Option<bool> = None;
    for p in list {
        if ret == Some(false) {
            return ret;
        }
        let matched = if p.isliteral {
            p.pattern.as_bytes() == name.as_bytes()
        } else {
            // SAFETY: pattern and name are valid C strings.
            unsafe { libc::fnmatch(p.pattern.as_ptr(), cname.as_ptr() as *const c_char, 0) == 0 }
        };
        if matched {
            // dir-only (isdir == true) lowers priority — file match (false) wins.
            ret = match (ret, p.isdir) {
                (Some(false), _) => Some(false),
                (_, false) => Some(false),
                (_, true) => Some(true),
            };
        }
    }
    ret
}

fn enter_into(list: &[&Pattern], name: &str, out: &mut Patterns) {
    let cname = match CString::new(name) {
        Ok(c) => c,
        Err(_) => return,
    };
    for p in list {
        let matched = if p.isliteral {
            p.pattern.as_bytes() == name.as_bytes()
        } else {
            unsafe { libc::fnmatch(p.pattern.as_ptr(), cname.as_ptr() as *const c_char, 0) == 0 }
        };
        if matched {
            if let Some(sub) = p.sub.as_deref() {
                if sub.sub.is_none() {
                    out.leaf.push(unsafe { std::mem::transmute::<&Pattern, &'static Pattern>(sub) });
                } else {
                    out.branch.push(unsafe { std::mem::transmute::<&Pattern, &'static Pattern>(sub) });
                }
            }
        }
    }
}

/// Owns all parsed patterns and exposes the root match contexts.
#[derive(Default, Debug)]
pub struct ExcludeSet {
    storage: Vec<Box<Pattern>>,
    root: Patterns,
    root_unanchored: Patterns,
}

impl ExcludeSet {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.root.isroot = true;
        s
    }

    pub fn add(&mut self, pattern: &str) {
        if pattern.is_empty() {
            return;
        }
        let anchored = pattern.starts_with('/');
        let parsed = Pattern::parse(pattern);
        // SAFETY: we move into `storage` so addresses are stable. We never
        // remove from storage, and `Patterns` borrows are 'static via transmute
        // bounded by ExcludeSet's lifetime in practice.
        self.storage.push(parsed);
        let last = self.storage.last().unwrap().as_ref();
        let last_static: &'static Pattern =
            unsafe { std::mem::transmute::<&Pattern, &'static Pattern>(last) };
        let target = if anchored {
            &mut self.root
        } else {
            &mut self.root_unanchored
        };
        if last_static.sub.is_some() {
            target.branch.push(last_static);
        } else {
            target.leaf.push(last_static);
        }
    }

    /// Patterns for the absolute path's level. Slow — call once per scan root,
    /// not per file. Subdirectory descent should use [`Patterns::enter`].
    pub fn for_path(&self, path: &str) -> Patterns {
        let mut path = path.trim_matches('/');
        if path.is_empty() {
            return self.root.clone();
        }
        let mut cur = self.root.clone();
        while let Some(idx) = path.find('/') {
            let name = &path[..idx];
            path = &path[idx + 1..];
            cur = cur.enter(self, name);
        }
        cur.enter(self, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_test_foo(set: &ExcludeSet, p: &Patterns) {
        assert_eq!(p.matches(set, "root"), None);
        assert_eq!(p.matches(set, "bar"), Some(false));
        assert_eq!(p.matches(set, "qoo"), Some(false));
        assert_eq!(p.matches(set, "xyz"), Some(false));
        assert_eq!(p.matches(set, "okay"), None);
        assert_eq!(p.matches(set, "somefile"), Some(false));
        let s = p.enter(set, "okay");
        assert_eq!(s.matches(set, "bar"), None);
        assert_eq!(s.matches(set, "xyz"), None);
        assert_eq!(s.matches(set, "notokay"), Some(false));
    }

    #[test]
    fn parse_empty_pattern_skipped() {
        let mut set = ExcludeSet::new();
        set.add("");
        assert!(set.root.leaf.is_empty());
        assert!(set.root.branch.is_empty());
    }

    #[test]
    fn parse_anchored_dir_pattern() {
        // "//a//" → trim leading; "a/" — dir-only top component, no sub.
        let p = Pattern::parse("//a//");
        assert_eq!(p.pattern.to_bytes(), b"a");
        assert!(p.isdir);
        assert!(p.isliteral);
        assert!(p.sub.is_none());
    }

    #[test]
    fn parse_chained_pattern() {
        let p = Pattern::parse("foo*/bar.zig");
        assert_eq!(p.pattern.to_bytes(), b"foo*");
        assert!(p.isdir);
        assert!(!p.isliteral);
        let s = p.sub.as_deref().unwrap();
        assert_eq!(s.pattern.to_bytes(), b"bar.zig");
        assert!(!s.isdir);
        assert!(s.isliteral);
        assert!(s.sub.is_none());
    }

    #[test]
    fn matches_upstream_battery() {
        // Ported from exclude.zig `test "Matching"`.
        let mut set = ExcludeSet::new();
        for pat in [
            "/foo/bar",
            "/foo/qoo/",
            "/foo/qoo",
            "/foo/qoo/",
            "/f??/xyz",
            "/f??/xyz/",
            "/*o/somefile",
            "/a??/okay",
            "/roo?",
            "/root/",
            "excluded",
            "somefile/",
            "o*y/not[o]kay",
        ] {
            set.add(pat);
        }

        let a0 = set.for_path("/");
        assert_eq!(a0.matches(&set, "a"), None);
        assert_eq!(a0.matches(&set, "excluded"), Some(false));
        assert_eq!(a0.matches(&set, "somefile"), Some(true));
        assert_eq!(a0.matches(&set, "root"), Some(false));
        let a1 = a0.enter(&set, "foo");
        assert_test_foo(&set, &a1);

        let b0 = set.for_path("/somedir/somewhere");
        assert_eq!(b0.matches(&set, "a"), None);
        assert_eq!(b0.matches(&set, "excluded"), Some(false));
        assert_eq!(b0.matches(&set, "root"), None);
        assert_eq!(b0.matches(&set, "okay"), None);
        let b1 = b0.enter(&set, "okay");
        assert_eq!(b1.matches(&set, "excluded"), Some(false));
        assert_eq!(b1.matches(&set, "okay"), None);
        assert_eq!(b1.matches(&set, "notokay"), Some(false));

        let c0 = set.for_path("/foo/");
        assert_test_foo(&set, &c0);
    }
}
