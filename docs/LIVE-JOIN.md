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
the run's log.

## What M2 adds

The same converter and builder, fed by rove's live session instead of a file:
`allocation`/`live_session` ported for the current gamejoin shape (no udmux,
`UdmuxEndpoints` → `NetStackConfig` cert pins, zero-token path), the effect
enum consumed on the network thread, replication ops converted on the network
thread and drained on the gameplay thread under the 2 ms budget, streaming
interest forwarded to rove (`replication_out::replication_focus`), and the
`LocalPlayer` created by `create_player` replaced by the server's player — one
player, one world, one frame loop.
