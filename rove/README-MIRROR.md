# `rove/`: rove, kept as the network reference

This directory is **rove pinned at `70b24c3b7d94`** (`alexkkork/rove`, the
commit whose gamejoin shape this client follows), copied in unmodified except
for the removals listed below. It is not a workspace member and nothing in the
app builds from it: `cargo check --workspace` never enters it. It is here so
the bridge work (M1–M6) can read the reference decoder, transport and session
code beside the client it is being ported into, with `git log`/`git blame`
intact for anything cited from it.

The rules for what may be taken from it are in `docs/LIVE-JOIN.md`: the network
backend only (`client-core` plus the transport/session pieces of
`rust-client`), never the executor shell, never a second Luau VM, never its
`ui/`, `terminal/` or `bin/rove-exec`.

## What was removed, and why

| Removed | Reason |
| --- | --- |
| `ui/`, `terminal/`, `bin/rove-exec` | The executor/browser-preview front ends. Not part of a game client. |
| `rust-client/src/rust_host/`, `src/main.rs`, `src/bin/`, `decompiler.rs`, `ui_server.rs`, `session_ui.rs`, `dashboard.rs`, `join_cli.rs`, `engine_bridge.rs`, `native_legacy.rs` | The headless-Luau *host*: its own VM owner thread, script catalog executor, decompiler and local dev UI. The client uses rustudio's Luau runtime as the single VM, so none of this is portable — keeping it would invite porting a second VM by accident. |
| `tools/`, `examples/`, `scripts/` | rove's own CLI harness tools and smoke scripts; they import the removed host. |
| `vendor/roblox-player-scripts-745/API-Dump.json` | Byte-identical in role to the root `api-dump.json` (both are Roblox's 0.745 dump; root copy is the newer 955-class build). `provenance.json` keeps its SHA-256, and `rove/THIRD_PARTY.md` still records where it came from. |
| `runtime/`, `stellar-sandbox/` | rove's C++/Rust VM runtime, and the executor sandbox. The one file anything referenced, `runtime/class-parents-745.json`, is preserved at `reference/class-parents-745.json`. |

Everything else — `client-core/` (transport, replication decoder, `WorldStore`,
schema, session core), `rust-client/`'s network/session/physics-prediction
modules, `rust-runtime-sys/` (pinned for reference only), `tests/fixtures/`,
`inputs/`, `vendor/` (patched `quinn-proto`, `wtransport`, `luau`) — is as
rove had it.

Assertions in `client-core`'s tests that referenced removed paths were left in
place rather than edited: this copy is kept byte-for-byte where it still
exists, so a diff against upstream `alexkkork/rove@70b24c3` shows only the
deletions above. `docs/LIVE-JOIN.md` records the exact revision and each file
the port actually uses.
