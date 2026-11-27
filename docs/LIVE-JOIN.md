# Live join: the bridge design

**Status:** M1 in progress (offline half built). The live half is M2.
**Pinned:** rove `alexkkork/rove@70b24c3b7d94` (copied under `rove/`, see
`rove/README-MIRROR.md`); rustudio `TouseefX/rustudio-android@7917ac1`
(v0.5.2) is this repository's tree. M0's reading of both is
`docs/M0-UNDERSTANDING.md`.

## What is being joined, and where the seam is

The client is rustudio: its `Document`, its renderer, its physics, its one Luau
runtime (`crates/rustudio-engine`), its Android app. rove supplies only the
network backend: the gamejoin parse (`allocation.rs`), the WebTransport/QUIC
transport, the replication decoder and the live-session state machine.

The seam is a **bridge** that turns rove's decoded replication into mutations of
the client `Document` through the same journalled path rustudio's own Play
replication already uses (`crates/rustudio-engine/src/replicate.rs`, from
`RepMap` and `RepOp` to `Runtime::apply_*`). There is no second world and no
second VM.

```
      rove (network thread)                bridge                         rustudio (gameplay thread)
 ┌──────────────────────────┐   ┌───────────────────────────┐   ┌──────────────────────────────────┐
 │ QUIC + session (tokio)   │   │ Effect/op conversion      │   │ step_play                       │
 │ ReplicationDecoder       │──▶│ rove id → Ref             │──▶│  RepMap-style bounded drain     │
 │ WorldStore (snapshot)    │   │ `$`-tag → Variant (once)  │   │  Runtime::apply_* → mark_prop    │
 │ Effect::{Pings,Rpc,…}    │   │ bounded channel (backlog) │   │  rt.dispatch() → scene.sync      │
 └──────────────────────────┘   └───────────────────────────┘   └──────────────────────────────────┘
```

## Threading

- **One network thread** owns the tokio runtime, the QUIC connection, rove's
  decoder and its world store. It never touches a `Document`, a Lua state or the
  renderer. This is the `PlayWorker` pattern (`play_worker.rs:13`): a thread owns
  its state for its whole lifetime and publishes plain data.
- **One bounded channel** carries converted work to the gameplay thread
  (`mpsc::sync_channel`, as `PlayWorker::submit` uses one for its jobs). The
  bound is the backpressure: when replication outruns the frame budget the
  network thread waits rather than the queue growing without limit.
- **The gameplay thread** drains inside `step_play`, where `replicate_with`
  drains `World.rep` today, under the same `RepMap::begin_frame` budget
  (15% of the frame-time EMA, floored at 1.6 ms, capped at 48 ms —
  `replicate.rs:168`). The M1 budget target is 2 ms/frame and it is a
  parameter, not a constant: `RepMap.frame_budget`.
- Rendering, physics and every Lua call stay on the gameplay thread, as they are
  today. Nothing in the bridge calls `mark_all()` after the initial world.

## The authoritative VM

One: rustudio's Luau runtime, in `crates/rustudio-engine`. rove's script
admission rules are ported into it — the catalog shape (`Script` /
`LocalScript` / `ModuleScript`, 128 KiB of code per entry, 16 MiB per catalog,
bytecode 13/14 type_version 3) and the eligibility checks — but **not** rove's
VM, its host shell or its executor entry points. rove's client-script
new-allocation logic is ported as policy; the execution is rustudio's.

## Client authority

What runs locally: client scripts, remotes (both directions), local movement
prediction and the GUI. What does not: server scripts (they stay on the server
and reach the client only through replication and remotes). Movement authority
is the server's; the client predicts and is corrected (`owned_part_*` in rove,
`rustudio-physics`' client joints here).

## What M1 built (this milestone)

`crates/rustudio-live`:

- `snapshot::parse` reads rove's world snapshot JSON exactly as
  `WorldStore::from_snapshot` does (`version`, `sequence`, `nodes`, every other
  key as metadata; `id`, `class_name`, `name`, `parent` — string, `$ref` object
  or null —, `properties`, `children`). Absent and null slots stay distinct, as
  in rove.
- `snapshot::ValueConverter` turns rove's `$`-tagged values into
  `rbx_types::Variant` **once, at the boundary**: `$vector3`, `$vector2`,
  `$cframe` (row-major rotation, as rove writes and reads it), `$udim`,
  `$udim2`, `$color3`, `$color3uint8`, `$rect`, `$brickcolor`, `$enum`,
  `$number`, `$string`/`$shared_string`/`$content`, `$bytes_hex`, `$faces`, and
  the payloads a place file also has (`$number_range`, `$number_sequence`,
  `$color_sequence`, `$font`) through `rustudio_core::project_values`. Anything
  else is refused by name — a property keeps its documented default and the
  refusal is counted and logged, never silently defaulted. `$ref` is the
  builder's job, because it needs the id map.
- `world::build` makes the client `Document` the way `replicate::initial_client`
  does: a fresh `WeakDom`, every node linked into a map of `Ref`s, services at
  the root, a child that arrives before its parent waiting for it, a ref to a
  node the world lacks dropped and counted, and `Players.LocalPlayer` plus
  `Player.Character` bound.
- `world::World::report` / `stats_json` are the numbers M1 is accepted on.

Opened from the app as `rustudio --live-snapshot FILE` (or by opening a file
named `world_snapshot.json`), and measured in CI by
`.github/workflows/live.yml`: the bridge's unit tests, a headless render of the
fixture, `--bench` frame statistics, and an assertion step — all printed into
the run's log and repeated as annotations, because a log is not always
readable back.

What the accepted run printed (lavapipe, a CPU rasteriser: the frame times are
a floor, not the phone's):

| | |
|---|---|
| bridge | 7 nodes → 8 instances in 0.1 ms; roots `Workspace, Players, RunService`; 2 tagged + 5 scalar values; refs bound 2; `LocalPlayer: FixturePlayer` |
| correctness | 22 bridge tests; dangling refs 0, refused values 0, orphans 0 |
| render | `fixture-world.png`, 1600×900, 97 658 bytes, 180 frames |
| frames | mean 38.63 ms headless (lavapipe); `--bench`: 300 frames, median 25.68 ms, p90 26.59, p99 35.68 |

Two limits the run makes plain. Lavapipe is slow enough that the frame time
says nothing about a phone; the bench's p99/max spread (35.7 / 290 ms) is the
software rasteriser's, not the player's. And the fixture is seven nodes: the
scale numbers (1818 ≈ 5.6k nodes, Phantom Forces ≈ 89k) are M3's to measure,
not M1's to claim.

## What M2 built (this milestone)

The live half, in `crates/rustudio-live/src/live.rs`: rove's own action
vocabulary applied to a world that is already being drawn.

| rove action | what the world does |
|---|---|
| `create` | one instance, under its parent if the world has it — else at the root, waiting |
| `set` | properties written through the same `ValueConverter` the snapshot path uses; `fields.Parent` is an edge; `$ref` binds when its target has arrived, else is counted |
| `remove` | the subtree is destroyed and every one of its ids forgotten, so no `$ref` can bind a dead ref |
| `event` | counted here, dispatched by the engine |

The rules M1 set are the rules here: a value is converted once at the boundary,
a child whose parent has not arrived waits instead of being re-parented twice, a
dangling `$ref` is dropped and counted, and classes come from `api-dump.json`.
Seven tests pin the cases that only a live stream produces: a child journaled
before its parent, a parent edge as a later `set`, a `$ref` before its target, a
repeat `create` (rove re-sends on rejoin), and a `remove` that takes a subtree
and its ids.

**The decoder is rove's, in the workspace.** `rove/client-core` is a member
(`rove/client-core`, `rove/client-core/src/session.rs`), and it is the same code
a live connection runs: `SessionCore::ingest` takes the payloads off the wire,
journals the receipt, and hands back the actions. Nothing in the replay path
re-derives them from a world afterwards, which is what would let a fixture agree
with itself.

Getting it there took three things that belong in the record, because each was a
silent mismatch rather than a missing file:

- **the lock.** Six crates (`pem`, `wtransport`, `wtransport-proto`,
  `x509-parser`, `httlib-huffman`, `octets`) had no entries, and this
  environment has no cargo. The repository resolves its lock in CI now
  (`ci.yml`'s `lockfile` job: `cargo fetch`, a minimal refresh that leaves pins
  alone, committed back with `contents: write`). 628 packages became 681.
- **the vendored manifests.** `rove/vendor/wtransport` and `vendor/epaint`
  declare example and bench targets whose files are not in the tree, and Cargo
  refuses to load such a manifest at all — the lock could not even be resolved
  until those target declarations were dropped.
- **rove's exact pins.** `rustls =0.23.37` leaves no slot for the root's
  `vendor/rustls` 0.23.46 patch, which is where `rbx_transport_capabilities`
  lives (accepted code, `rustudio-net/src/transport.rs:204`), and it drags
  `ureq` to a 3.4 that has no `timeout_per_read`. Three pins (rustls, tokio,
  bytes) now name the versions the rest of the workspace builds, still exact.

**The offline evidence.** `crates/rustudio-live/tests/live_replay.rs` replays
`tests/fixtures/live_session.json` through that path: five packets — schema
(DataModel, Workspace, Part), globals (the place's top containers), then
replication carrying a Part under Workspace, a Player and a second Part whose
parent edges arrive in a later packet, and the first Part destroyed. The test
asserts rustudio's world after the replay: the roots, a `Vector3` `Size` and a
`bool` `Anchored` (the packet's types, not JSON floats), the deferred parent edge
resolved, and the removal gone from the tree with its id forgotten. A truncated
replication packet is refused by the decoder and counted, never half applied.

**The order CI runs in.** The lock is the gate: `lockfile` (cargo fetch, minimal
refresh, committed back on a push) is a `needs` of every building job, and each of
them checks out `github.ref` rather than `github.sha`, because a refresh commit is
one ahead of the commit that started the run. So one push resolves the lock and
then builds the tree it describes — no dispatch, no second round trip. A pull
request is the one case CI cannot fix (read-only token), and `lockfile` says so
there instead of six jobs each failing on a lock they were told not to resolve.

macOS is the long pole: about an hour, because it is the only runner that runs
the whole test suite (`cargo test --workspace`). Nothing waits on it — every other
job runs in parallel — but its verdict arrives long after the rest.

**The run.** CI on this branch at the time of writing: `check (ubuntu-latest)`,
`check (windows-latest)`, `linux-release`, `windows-release`, `macos-app` and the
`lockfile` job are green — the two checks compile the whole workspace including
`rove-client-core` and `tests/live_replay.rs`, and the release jobs link the
tests. The macOS jobs (the two `check`es and `macos-app`) sit in the runner
backlog, which is where they were for M1 and the accepted M2 run as well. The
test step runs there, and only there: Windows `cargo test` has never finished in
this tree (26 minutes then a failure in the accepted run; a 35-minute bound here,
with rustc dying on an "N warnings" summary), and the tree's own M0 record says
tests run on macOS only.

**What the recording is not.** It is a hand-built payload whose ids the decoder
assigns, not a capture from a live server — no packet from Roblox has been
recorded yet. The bytes are checked against rove's own grammar (and decode back
cleanly), but the claim is "the replay path works", not "this is what 1818
sends".

## What M3 adds

The same converter and apply path, fed by a real connection instead of a
recording:

- **the join**: rove's `allocation.rs` ported for the current gamejoin shape —
  `/v1/join-game` and `/v1/join-game-instance`, `UdmuxEndpoints[0].Address` or
  `MachineAddress`, the port from `NetStackConfig`/`NetStackPort`, certificate
  pins (16 at most), and the zero-token path when `NetStackTokenValue` is absent.
  rustudio has the editor's Team Create join (`crates/rustudio-net`); this is the
  player's, which is the piece it does not have.
- **the transport**: WebTransport to the allocated endpoint, through rove's
  `transport` feature. `is_global_ipv4` refuses loopback, so an offline fake
  server needs an explicit test allowance rather than a bend in that check.
- **the frame loop**: `SessionCore::ingest` on the network thread, the journal
  drained on the gameplay thread under the same 2 ms budget `RepMap` uses, and
  the actions applied by `live::Live` — a burst is a hitch, not a freeze.
- **the player**: the server's player replaces the one a world builds itself —
  one `LocalPlayer`, one world, one frame loop.

## Scale, unmeasured

The fixture is seven nodes and the recording is a five-packet session that builds
four instances. The numbers M3
has to stand behind — 1818 ≈ 5.6k nodes, BedWars ≈ 3.4k, Phantom Forces ≈ 89k —
need a capture of each and a frame budget measured against them. Nothing here
claims them.
