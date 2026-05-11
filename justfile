set shell := ["bash", "-cu"]

bin := "./target/release/ncdu-rs"
default_path := env_var_or_default("PATH_TO_SCAN", justfile_directory())

default:
    @just --list

# Build release binary
build:
    cargo build --release

# Build debug binary
build-debug:
    cargo build

# Run all tests
test:
    cargo test

# Run a single test by name pattern
test-one PATTERN:
    cargo test {{PATTERN}} -- --nocapture

# Lint with clippy (warnings as errors)
lint:
    cargo clippy --all-targets -- -D warnings

# Format check
fmt-check:
    cargo fmt --check

# Apply formatting
fmt:
    cargo fmt

# Remove build artifacts
clean:
    cargo clean

# Install to ~/.cargo/bin
install:
    cargo install --path . --force

# Browse a directory interactively (default: this repo)
browse PATH=default_path: build
    {{bin}} {{PATH}}

# Parallel browse with N threads
browse-par PATH=default_path THREADS="8": build
    {{bin}} -t {{THREADS}} {{PATH}}

# Export JSON dump to file
export PATH=default_path OUT="dump.json": build
    {{bin}} -o {{OUT}} {{PATH}}
    @echo "wrote $(wc -c < {{OUT}}) bytes to {{OUT}}"

# Export JSON to stdout (head 40 lines)
peek PATH=default_path: build
    {{bin}} -o - {{PATH}} | head -40

# Scan with extended attrs (uid/gid/mode/mtime)
export-ext PATH=default_path OUT="dump-ext.json": build
    {{bin}} -e -o {{OUT}} {{PATH}}

# Scan with exclude patterns
export-excl PATH=default_path: build
    {{bin}} --exclude 'target/' --exclude '*.lock' -o /tmp/excl.json {{PATH}}
    @echo "wrote /tmp/excl.json ($(wc -c < /tmp/excl.json) bytes)"

# XFS quota fast-path (Linux only; needs xfsprogs + project quota)
xfs PATH=default_path: build
    {{bin}} --xfs-quota {{PATH}}

# Bench sequential vs parallel scan
bench PATH=default_path: build
    @echo "--- sequential (t=1)"
    @time {{bin}} -t 1 -o /tmp/seq.json {{PATH}}
    @echo "--- parallel (t=8)"
    @time {{bin}} -t 8 -o /tmp/par.json {{PATH}}
    @echo "--- output sizes"
    @ls -l /tmp/seq.json /tmp/par.json

# Round-trip: scan -> export -> import -> re-export, diff results
round-trip PATH=default_path: build
    {{bin}} -o /tmp/rt1.json {{PATH}}
    # Re-import + re-export via a tiny throwaway script using cargo run.
    @echo "round-trip JSON via cargo (not yet exposed as a flag; uses tests)"
    cargo test json_import::tests::round_trip_mixed_tree

# Show binary version
version: build
    {{bin}} --version

# Sanity: build, test, lint, fmt-check
ci: build test lint fmt-check
    @echo "ok"
