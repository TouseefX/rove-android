#!/bin/sh
# keep the last built frontend runnable while engine sources are being edited.
set -eu
ROVE_TERMINAL_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROVE_TERMINAL_TARGET="${ROVE_TERMINAL_TARGET:-${XDG_CACHE_HOME:-$HOME/.cache}/rove-terminal/target}"
ROVE_REBUILD=0
ROVE_REBUILD_WORKER=0
if [ "${1:-}" = '--rebuild' ]; then
    ROVE_REBUILD=1
    shift
elif [ "${1:-}" = '--rebuild-worker' ]; then
    ROVE_REBUILD=1
    ROVE_REBUILD_WORKER=1
    shift
fi
if [ "$ROVE_REBUILD" = 0 ] && [ -x "$ROVE_TERMINAL_TARGET/release/rove-terminal" ] && [ -x "$ROVE_TERMINAL_TARGET/release/rove-session-worker" ]; then
    exec "$ROVE_TERMINAL_TARGET/release/rove-terminal" "$@"
fi
# Prefer the installed native toolchain used to validate this frontend.
if [ -z "${ROVE_RUST_BIN:-}" ] && [ "$(uname -s)-$(uname -m)" = "Darwin-arm64" ] && [ -x "$HOME/.rustup/toolchains/1.92.0-aarch64-apple-darwin/bin/cargo" ]; then
    ROVE_RUST_BIN="$HOME/.rustup/toolchains/1.92.0-aarch64-apple-darwin/bin"
fi
if [ -n "${ROVE_RUST_BIN:-}" ]; then
    ROVE_CARGO="$ROVE_RUST_BIN/cargo"
    RUSTC="$ROVE_RUST_BIN/rustc"
    RUSTDOC="$ROVE_RUST_BIN/rustdoc"
    PATH="$ROVE_RUST_BIN:$PATH"
    export RUSTC RUSTDOC PATH
elif command -v cargo >/dev/null 2>&1; then
    ROVE_CARGO=$(command -v cargo)
elif [ -x "$HOME/.cargo/bin/cargo" ]; then
    ROVE_CARGO="$HOME/.cargo/bin/cargo"
else
    echo 'Rust is required for the first build. Install it from https://rustup.rs.' >&2
    exit 1
fi
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}"
export CARGO_BUILD_JOBS
mkdir -p "$ROVE_TERMINAL_TARGET"
if [ "$ROVE_REBUILD_WORKER" = 1 ] || [ ! -x "$ROVE_TERMINAL_TARGET/release/rove-session-worker" ]; then
    echo 'building the rove session worker…'
    if ! "$ROVE_CARGO" build --locked --release --features embedded-worker --manifest-path "$ROVE_TERMINAL_DIR/Cargo.toml" --target-dir "$ROVE_TERMINAL_TARGET" >"$ROVE_TERMINAL_TARGET/worker-build.log" 2>&1; then
        cat "$ROVE_TERMINAL_TARGET/worker-build.log" >&2
        exit 1
    fi
    cp "$ROVE_TERMINAL_TARGET/release/rove-terminal" "$ROVE_TERMINAL_TARGET/release/rove-session-worker.new"
    mv "$ROVE_TERMINAL_TARGET/release/rove-session-worker.new" "$ROVE_TERMINAL_TARGET/release/rove-session-worker"
fi
echo 'checking the rove terminal build…'
if ! "$ROVE_CARGO" build --locked --release --manifest-path "$ROVE_TERMINAL_DIR/Cargo.toml" --target-dir "$ROVE_TERMINAL_TARGET" >"$ROVE_TERMINAL_TARGET/terminal-build.log" 2>&1; then
    cat "$ROVE_TERMINAL_TARGET/terminal-build.log" >&2
    exit 1
fi
exec "$ROVE_TERMINAL_TARGET/release/rove-terminal" "$@"
