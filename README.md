# Rove — a native Roblox client for Android, in Rust

This repository is the native Android Roblox **client**: it joins a public
place, renders the world, runs the client's Luau, moves a character and draws
Roblox's own CoreGui. It is not an executor, not a script injector and not an
editor front end; there is no anti-cheat bypass anywhere in it.

It is two projects joined at a single seam:

- **[rustudio](README-RUSTUDIO.md)** (this tree, outside `rove/`) — the client
  shell: the Android app (`crates/rustudio-android`), the player viewport and
  GUI (`crates/rustudio-player`), the renderer, physics, the DataModel
  (`crates/rustudio-core`), the engine and its Luau runtime
  (`crates/rustudio-engine`), and the asset/account/Android plumbing.
- **[rove](rove/README-MIRROR.md)** (`rove/`) — the network backend reference,
  pinned at `alexkkork/rove@70b24c3b7d94`: the gamejoin parsing for the current
  join shape, the WebTransport/QUIC transport, the replication decoder and the
  live-session state machine. Only its `client-core` and its session/transport
  code are ported; its VM, executor shell, terminal and browser UI are not.

The seam is a bridge that turns rove's replication into DataModel mutations
through the same bounded, journalled path rustudio's own Play replication uses —
one Luau VM, one world, one frame loop. Design and ground rules:
[`docs/LIVE-JOIN.md`](docs/LIVE-JOIN.md); milestone notes:
[`docs/M0-UNDERSTANDING.md`](docs/M0-UNDERSTANDING.md).

## Building

Everything is built in CI; see `.github/workflows/android.yml` (APKs for
arm64 and x86_64) and `.github/workflows/ci.yml` (desktop checks and the
headless render harness). rustudio's own documentation —
[`README-RUSTUDIO.md`](README-RUSTUDIO.md), `docs/DEVELOPING.md`,
`docs/ANDROID.md` — describes the editor, the harness and the Android build
in the detail it was written with.

## Licence

rustudio: MIT OR Apache-2.0. rove: see `rove/LICENSE-rove`. Third-party
notices: `rove/THIRD_PARTY.md` and rustudio's bundled crate licences.
