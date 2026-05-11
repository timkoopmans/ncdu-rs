# ncdu-rs

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

Pre-alpha. Scaffold only.

## License

BSD-2-Clause, matching ncdu upstream. See [LICENSE](LICENSE).

## Credits

Original ncdu by Yorhel — <https://dev.yorhel.nl/ncdu>. Source: <https://code.blicky.net/yorhel/ncdu>.
