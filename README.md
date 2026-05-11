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

Alpha. Working CLI that scans + emits ncdu v2 JSON. Browser TUI in progress.

## Implemented

- `model` — arena tree (EntryId indices, not raw pointers)
- `json_export` / `json_import` — byte-compatible with ncdu v2 wire format
- `sink` — in-memory build, explicit `finalize()` instead of upstream's atomic refcount
- `scan` — single-threaded `std::fs` walker with `MetadataExt` for dev/ino/nlink/blocks
- `exclude` — glob patterns via libc `fnmatch`, anchored + unanchored, dir-only
- `delete` — recursive disk delete + tree prune + ancestor total recalc

## Not yet implemented (deferred)

- **Binary export/import format** (`bin_export.zig` + `bin_reader.zig`, ~1000 LOC).
  Skipped because the JSON format already covers persistence and the binary
  format only matters for very large dumps where compression and seekability help.
  Will be revisited if user demand or large-tree use cases appear.
- **zstd compression** of JSON dumps. Same rationale as bin.
- **Parallel scanner**. v0 is single-threaded; jwalk-style parallelism is on
  the roadmap.
- **Non-UTF-8 filenames in JSON import**. Requires a custom byte-level parser
  to replace `serde_json::Value`.
- **Streaming sink during scan**. Currently materialises a `Tree` then exports.
- **Hardlink stats dedup**. Hardlinked files count once per link in v0 totals;
  bounded but inflated when hardlinks are heavy.

## License

BSD-2-Clause, matching ncdu upstream. See [LICENSE](LICENSE).

## Credits

Original ncdu by Yorhel — <https://dev.yorhel.nl/ncdu>. Source: <https://code.blicky.net/yorhel/ncdu>.
