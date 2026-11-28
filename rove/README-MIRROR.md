# `rove/`: rove, kept as the network reference

The upstream reference is **`alexkkork/rove` at `3c375c274696`**. This is a
selective mirror, not a complete copy: the removals below and workspace-specific
adaptations are deliberate. Most retained source files match that revision;
exceptions include `client-core/Cargo.toml`, `client-core/src/session.rs`,
`client-core/src/wire.rs`, `rust-client/src/lib.rs`, `rust-client/src/live_session.rs`
and the reference-only `rust-client/Cargo.lock`. The reference-only files
`rust-client/src/asset_crypto.rs`, `live_physics.rs` and `live_runtime.rs` also
contain local safety fixes described below. `wire.rs` omits upstream's opt-in
raw packet-tail dump, which could expose network data.

It was first based on `70b24c3b7d94`; selected files were then updated to
`3c375c27`, and later divergent retained reference files were synchronized.
Some changes represented by that upstream revision:

| file | the change |
|---|---|
| `client-core/src/instance_state.rs`, `session.rs` | **lenient ISR decoding**: a record that fails part-way is dropped instead of the whole packet (upstream measured 99.6% acceptance on a live server). This is *the* fix for "logged in, receiving nothing". |
| `client-core/src/property_storage.rs` | 16 KiB pages instead of 64 KiB: a cold read maps only what it needs |
| `client-core/src/replication.rs`, `runtime_delta.rs` | `pub(crate)` visibility for helpers the delta path shares; unknown delta payloads keep their kind |
| `rust-client/src/allocation.rs` | the `User-Agent` names a **live** client version (`client_version.rs`), `Accept-Language` and `Roblox-Browser-Asset-Request`; a status-12 refusal refetches the version once |
| `rust-client/src/live_session.rs` | `RandomSeed1/2` seeding the asset keys, character-resolved evidence, owned-part uploads |
| `rust-client/src/client_version.rs`, `asset_crypto*.rs` | new: the version fetch and the asset-key registry |

The env-var switches upstream added (`ROVE_LENIENT_ISR`, `ROVE_LIVE_SESSION`)
are **not** used. This copy of `client-core/src/session.rs` takes the policy as
a constructor argument instead (`SessionCore::with_limits_live`), so a live join
is lenient and a replayed recording is strict, whatever the machine's
environment holds. `rove/client-core` **is** a workspace member used by the
live and join crates. `rove/rust-client` is reference-only, except where
individual files are explicitly included by offline comparisons. Synchronizing
its files does not by itself add their behavior to the Android app; that needs
a separate, tested port. This is a selective copy rather than upstream git
history; use upstream's history and the pinned revision when citing changes.

The reference-only safety changes do not infer solid terrain from its size or
teleport overlapping characters, use AES-192 for 192-bit keys, avoid treating
unregistered assets as encrypted merely because a default key exists, and
preserve supplied input capabilities with a platform fallback. These are
conservative choices: without terrain collision data, terrain support is not
simulated, and default-key-only encrypted assets need verified identification
before they can be decrypted. The standalone upstream client is incomplete in
this mirror, so its tests must be run in a complete upstream checkout.

The rules for what may be taken from it are in `docs/LIVE-JOIN.md`: the network
backend only (`client-core` plus the transport/session pieces of
`rust-client`), never the executor shell, never a second Luau VM, never its
`ui/`, `terminal/` or `bin/rove-exec`.

## What was removed, and why

| Removed | Reason |
| --- | --- |
| `ui/`, `terminal/`, `bin/rove-exec` | The executor/browser-preview front ends. Not part of a game client. |
| `rust-client/src/rust_host/`, `src/main.rs`, `src/bin/`, `decompiler.rs`, `ui_server.rs`, `session_ui.rs`, `dashboard.rs`, `join_cli.rs`, `engine_bridge.rs`, `native_legacy.rs` | The headless-Luau *host*: its own VM owner thread, script catalog executor, decompiler and local dev UI. The client uses rustudio's Luau runtime as the single VM, so none of this is portable — keeping it would invite porting a second VM by accident. |
| `tools/` (except `tools/fixtures/`), `scripts/` | rove's own CLI harness tools and helper scripts; they import the removed host. `tools/fixtures/rust-rewrite/compact/snapshot.json` is kept: `live_runtime.rs`, `runtime_owner.rs` and several tests `include!` it. |
| `examples/` (the `.luau` smoke scripts) | Kept, actually: `rust-client`'s tests `include_str!` them (`bulk_move`, `joint_poses`, `animation_*`, `part_queries`, …). They are test inputs, not the executor. |
| `vendor/roblox-player-scripts-745/API-Dump.json` | Byte-identical in role to the root `api-dump.json` (both are Roblox's 0.745 dump; root copy is the newer 955-class build). `provenance.json` keeps its SHA-256, and `rove/THIRD_PARTY.md` still records where it came from. |
| `runtime/` (C++/CMake) and `stellar-sandbox/` | rove's C++ VM runtime and the executor sandbox. **The `runtime/*.json` and `runtime/*.inc` reference tables are kept** (`class-parents-745.json`, `class-constructors-745.json`, `physics-constructors-745.json`, `physics-initial-velocities-745.json`, `owned-physics-constructors-745.json`, and the `.inc` tables): `rust-client`'s decoders `include_str!` them, and they are the 0.745 class/physics metadata the port reads. `runtime/vendor/` (Luau, nlohmann provenance) is kept for the same reason. Only the C++ translation units and CMake build are gone. |

`ui/keyboard-codes.json` is likewise kept: `rust-client/src/input_bridge.rs`
`include_str!`s it, and it is the key mapping M4 needs.

Everything else — `client-core/` (transport, replication decoder, `WorldStore`,
schema, session core), `rust-client/`'s network/session/physics-prediction
modules, `rust-runtime-sys/` (pinned for reference only), `tests/fixtures/`,
`inputs/`, `vendor/` (patched `quinn-proto`, `wtransport`, `luau`) — is as
rove had it.

Assertions in `client-core`'s tests that referenced removed paths were left in
place rather than edited. `docs/LIVE-JOIN.md` describes the files used by the
port; the exceptions above mean the mirror should not be assumed byte-identical
without checking.
