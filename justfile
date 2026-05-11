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

# Bootstrap + bench ncdu-rs on a cdc node. Per cluster-mutating-recipe rule,
# requires CONFIRM=yes. Self-contained (no scripts/ dep). Idempotent.
# Results land in ~/ncdu-rs/data/ on the node; pull with `just cdc-fetch`.
# Usage:
#   CONFIRM=yes just cdc-setup
#   CONFIRM=yes just cdc-setup cdc-2 /tank/raw/hyperliquid/trades
cdc-setup HOST="cdc-1" SCAN_PATH="$HOME":
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "${CONFIRM:-}" != "yes" ]; then
        echo "Refusing without CONFIRM=yes (cluster-mutating recipe)."
        echo "Re-run: CONFIRM=yes just cdc-setup {{HOST}} {{SCAN_PATH}}"
        exit 1
    fi
    ssh -A {{HOST}} bash -s -- {{SCAN_PATH}} <<'REMOTE'
    set -euo pipefail
    SCAN_PATH="$1"
    if ! command -v rustup >/dev/null 2>&1; then
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
            | sh -s -- -y --default-toolchain stable --profile minimal
    fi
    . "$HOME/.cargo/env"
    # Ratatui + transitive deps need rustc >= 1.88; refresh stable.
    rustup update stable
    cd "$HOME"
    if [ -d ncdu-rs/.git ]; then
        git -C ncdu-rs pull --ff-only
    else
        git clone git@github.com:timkoopmans/ncdu-rs.git
    fi
    cd ncdu-rs
    mkdir -p data
    cargo build --release
    BIN=./target/release/ncdu-rs
    SAFE=$(echo "$SCAN_PATH" | tr / _ | sed 's,^_,,')
    /usr/bin/time -f '%e s wall, %M KB peak rss' \
        "$BIN" -t 1 -o "data/${SAFE}.seq.json" "$SCAN_PATH" \
        2> "data/${SAFE}.seq.time"
    /usr/bin/time -f '%e s wall, %M KB peak rss' \
        "$BIN" -t 8 -o "data/${SAFE}.par.json" "$SCAN_PATH" \
        2> "data/${SAFE}.par.time"
    echo "--- seq:" ; cat "data/${SAFE}.seq.time"
    echo "--- par:" ; cat "data/${SAFE}.par.time"
    if stat -f -c %T "$SCAN_PATH" 2>/dev/null | grep -qi xfs \
        && sudo -n true 2>/dev/null
    then
        sudo "$BIN" --xfs-quota "$SCAN_PATH" | tee "data/${SAFE}.xfs.txt"
    fi
    REMOTE

# Pull benchmark results from a cdc node into local ./data/ for inspection.
cdc-fetch HOST="cdc-1":
    mkdir -p data
    rsync -avz --progress {{HOST}}:~/ncdu-rs/data/ ./data/
