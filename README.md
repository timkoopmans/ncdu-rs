# ncdu-rs

![100% Vibe Coded](https://img.shields.io/badge/100%25-vibe%20coded-ff69b4?style=for-the-badge)

Rust port of [ncdu](https://dev.yorhel.nl/ncdu) v2 — parallel disk usage analyzer with a terminal UI.

## Goals

- Behavioural parity with ncdu v2 (Zig upstream by Yorhel)
- Same JSON export/import format
- Parallel directory walker
- `ratatui` + `crossterm` for the UI (no ncurses dependency)
- Track upstream ncdu v2 features

## Non-goals (initial)

- Windows support
- Extending the JSON schema

## Future / divergence

- XFS project-quota fast-path (`xfs_quota report` when available, skip the walk)
- ZFS snapshot awareness
- Network-mount aware concurrency throttle

## Status

Alpha. Working CLI: scans, browses interactively, exports/imports ncdu v2 JSON,
deletes, supports parallel scan and the XFS-quota fast-path on Linux.

## Usage

```bash
# Interactive browser
ncdu-rs /var/log

# Parallel scan with 8 worker threads
ncdu-rs -t 8 /

# Export ncdu v2 JSON dump
ncdu-rs -o dump.json /var/log

# XFS project-quota fast-path (Linux; needs xfsprogs + project quota set up)
ncdu-rs --xfs-quota /tank/dataset

# Glob exclude (repeatable)
ncdu-rs --exclude '*.log' --exclude 'node_modules/' .
```

## Implemented

- `model` — arena tree (EntryId indices, not raw pointers)
- `json_export` / `json_import` — byte-compatible with ncdu v2 wire format
- `sink` — in-memory build with explicit `finalize()` (vs upstream atomic refcount)
- `scan` — single-threaded `std::fs` walker with `MetadataExt` for dev/ino/nlink/blocks
- `scan_parallel` — rayon two-phase walker (parallel I/O, sequential Tree build)
- `exclude` — glob patterns via libc `fnmatch`, anchored + unanchored, dir-only
- `delete` — recursive disk delete + tree prune + ancestor total recalc
- `browser` — ratatui TUI: hjkl/arrow nav, enter/bksp descend/ascend, d delete (w/ confirm), q quit
- `xfs_quota` (Linux) — detects XFS + project ID, calls `xfs_quota report -np`, returns O(1) byte total

## Differentiator vs upstream ncdu

`--xfs-quota` reads the kernel's project-quota counters instead of walking. On
a 50M-file XFS volume this turns a multi-minute scan into a microsecond
syscall. Upstream ncdu never consults XFS quotas.

## Not yet implemented (deferred)

- **Binary export/import format** (`bin_export.zig` + `bin_reader.zig`, ~1000 LOC).
  JSON covers persistence; binary only matters for very large dumps where
  compression and seekability help. Revisit on demand.
- **zstd compression** of JSON dumps. Same rationale.
- **Non-UTF-8 filenames in JSON import**. Requires a custom byte-level parser
  to replace `serde_json::Value`.
- **Streaming sink during scan**. Currently materialises a `Tree` then exports.
- **Hardlink stats dedup**. Hardlinked files count once per link in totals;
  bounded but inflated when hardlinks are heavy.
- **TUI extras**: sort column switching, help/info/refresh/search overlays,
  show-hidden toggle, hardlink shared-count column.
- **ZFS snapshot awareness**, **network-mount aware concurrency throttle** —
  follow-ups beyond ncdu parity.

## Testing

```bash
cargo test
```

37 unit + integration tests covering model, JSON round-trip, sink, scan,
parallel scan, exclude (with the full upstream `test "Matching"` battery),
delete, and the XFS quota report parser.

## License

BSD-2-Clause, matching ncdu upstream. See [LICENSE](LICENSE).

## Credits

Original ncdu by Yorhel — <https://dev.yorhel.nl/ncdu>. Source: <https://code.blicky.net/yorhel/ncdu>.
