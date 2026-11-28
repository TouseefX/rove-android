# M0 — Understanding

**Milestone:** M0 (no code). **Branch:** `arena/09b28e1f-rove-android`.
**Bases:** rustudio `TouseefX/rustudio-android` `main` `7917ac1` (v0.5.2, now the
whole tree of this repo) and rove `alexkkork/rove` `main`
`70b24c3b7d94`, copied verbatim under `rove/` (see `rove/README-MIRROR.md`).

Everything below is cited from the tree in this repository. Line numbers are of
this commit; `crates/…` and `src/…` mean rustudio's, `rove/…` means rove's.
Anything I have not verified is marked **unverified**.

Answers to Part A (rustudio) are in §1–§5, Part B (rove) in §6–§9, then the
consequences that fix M1–M7 (§10).

---

## 1. What a Play session actually is

**Q: How does rustudio start Play, and what is the client DataModel relative to
the server one?**

`play.rs` keeps two `Side`s (`rustudio-engine/src/runtime.rs:64` `enum Side`),
and each side is a `Runtime` plus a `World`:

```rust
// crates/rustudio-engine/src/runtime.rs:141
pub struct World {
    pub doc: Option<Document>,   // this side's DataModel
    pub side: Side,
    pub has_peer: bool,          // a separate client exists: remotes are delivered
    pub rep: Vec<RepOp>,         // server changes waiting to reach the client
    pub outbox: Vec<RemoteMsg>,
    pub realtime: bool,
    // …
}
```

Start-up order for a play session in `crates/rustudio-player/src/play.rs`:

1. `new_side(mode, side, peer, signal_mode)` (`play.rs:674`) builds a
   `Runtime::new()` + `install_side()` + `seed_vm`, then
   `World::new(Document::empty(), mode)` — the document is *not* the editor's
   (`doc = None` until one is swapped in).
2. `Game::with_side` (`play.rs:764`) swaps a `Document` in and out of
   `self.doc`/`self.other_doc` and tags output lines `[Server]`/`[Client]`.
3. `Document::clone_for_play(title)` (`crates/rustudio-core/src/doc.rs:1160`)
   deep-copies the whole edit DataModel for the **server** side (`WeakDom::new`,
   `dom.reserve(self.dom.len())`, `clone_multiple_into_external`, root renamed
   `Game`).
4. `doc.ensure_services()` (`doc.rs:1254`) adds `mech::ALWAYS_SERVICES`
   (`crates/rustudio-core/src/mech.rs:51`).
5. `rustudio_core::default_scripts::insert_player_scripts(&mut server_doc)`
   (`play.rs:1000` → `default_scripts.rs:390`): the real `PlayerModule` from
   Studio content if present, else `insert_own_loader`.
6. `replicate::initial_client(&server, title)`
   (`crates/rustudio-engine/src/replicate.rs:396`) builds the **client**
   document: a fresh `WeakDom` whose root is a copy of the server root, then a
   walk of the server root's children that copies each subtree
   (`copy_subtree`, `replicate.rs:267`), skips nothing but `SERVER_ONLY`
   (`replicate.rs:19` — `ServerStorage`, `ServerScriptService`, which arrive as
   empty instances because their classes *do* exist on the client), and fills
   `RepMap.s2c`/`c2s` (`RefMap`) so every server `Ref` has a client `Ref`.
7. `Document::from_play_dom(dom, title)` (`doc.rs:1138`) wraps it as a
   `DocKind::Place` document and bumps `revision`.

So the client DataModel is a **separate `Document` with its own `Ref`s**, made
by `initial_client` and kept in sync by `RepMap`. That is exactly the shape a
network bridge must reproduce: rove IDs become `Ref`s in a client document that
`initial_client`-style code builds, and every later change arrives as an op.

**Consequence for M1:** the M1 fixture path does not need the server side at
all — build a client `Document` from rove's snapshot, hand it to
`rustudio-player`'s harness as the document to render.

## 2. The change journal and its marks (the performance contract)

`crates/rustudio-core/src/doc.rs:179`:

```rust
pub struct Document {
    pub revision: u64,
    pub tree_revision: u64,
    pub style_revision: u64,
    journal: Journal, // :146 — base, marks, query, styles, read: [Option<u64>; 26], graph_pos
    pub dom: WeakDom,
    // …
}
```

The journal is append-only; readers hold *positions*, not queues. The API the
bridge must use and never abuse:

| API | `doc.rs` | Meaning |
| --- | --- | --- |
| `mark_prop(r, name: Ustr)` | 856 | one property changed on `r` |
| `mark_tree(r)` | 870 | `r`'s subtree changed structurally |
| `mark_all()` | 879 | **bumps `base`**: every reader's next `marks_for` returns `Marks::All`, i.e. a full scene rebuild |
| `marks_for(reader) -> Marks` | 938 | the reader's pending marks (pull; coalesced) |
| `marks_since(pos)` / `graph_marks()` / `journal_end()` | 992 / 1000 / 986 | read-only views |
| `discard_marks_for(reader)` | 953 | drop the marks a reader already knows (its own writes); the private `advance_mark_reader` (`:960`) is what moves the position, and a reader further behind than `JOURNAL_READER_LAG` has its position set to `None` (it rebuilds) |

Reader indices are per-consumer (`doc.rs:79`: `Scene=0, Physics=1, Query=2,
Project=3, ClientJoints=4, Gui=5, Children=6, Lights=7, ClientPhysics=8,
Effects=9, TeamCreate=10, Tags=11, Media=12, Anim=13, Layout=14,
SolverJoints=15, VfScene=16, Plugins=17, PluginGui=18, Highlights=19, Npcs=20,
RenderSnapshot=21, AnimatorBindings=22, ServerJoints=23, EditAudio=24,
Modal=25`; `JOURNAL_READERS = 26`): the renderer's scene reader,
the GUI reader, the physics reader and so on each have their own. `mark_all()`
is therefore *not* a cheap "invalidate my thing": it forces every consumer of
the document to rebuild everything.

Limits: `mech::JOURNAL_LIMIT = 1_000_000` (`mech.rs:390`),
`mech::JOURNAL_READER_LAG = 200_000` (`mech.rs:420`). A reader that falls
further behind than `JOURNAL_READER_LAG` is told to rebuild (`doc.rs:897`,
`doc.rs:967`); past `JOURNAL_LIMIT` entries the journal itself collapses
(`doc.rs:927`). A bridge that enqueues a big world's initial contents one op at
a time and lets the renderer lag is therefore *guaranteed* to end in a full
rebuild — the exact failure mode the performance rules forbid.

**Rules this fixes:**
- `mark_all()` at most once, when the initial world is ready (`§10.2`).
- Every op must go through the same `mark_prop`/`mark_tree` path
  `replicate.rs` uses, so the renderer sees one coalesced change per frame
  rather than a rebuild.
- Bounded batches per frame (`2 ms` start), because journal growth is bounded by
  *ops applied*, not by *time spent*.

## 3. The replication path the bridge must join

`crates/rustudio-engine/src/replicate.rs` is the reference implementation of
"server document changed → client runtime updated → marks → scene".

- `RepMap` (`replicate.rs:33`): `s2c`/`c2s` `RefMap`s, `streaming`,
  `queue: VecDeque<Pending>`, `queued_props`, `held` (server→client remotes held
  behind the replication that preceded them), `pending_refs` (client property
  pointing at a server `Ref` that has not arrived), `arrived`, `anim_dirty`,
  `buffered`, `frame_ema_us`.
- `begin_frame(dt)` (`replicate.rs:168`) computes this frame's budget as
  Studio's `ProcessPacketsJob` does: `HEALTHY_PERCENT = 15` (`:89`) of the
  frame-time EMA, floored at `MIN_US = 1600` (`:90`), capped at
  `MAX_FRAME_US = 48_000` (`:91`); if the oldest queued op is older than
  `MAX_DELAY = 100 ms` (`:92`) it catches up at
  `min(waiting, STEPS_PER_CYCLIC = 5) × max(dt·60, 1)`, capped by
  `JOB_SCALING = 10`, in `STEP_S = 1/120 s` steps.
- `apply` / `apply_budgeted` (`:525`, `:533`) enqueue then `drain`
  (`:539`), which stops at the budget and leaves the rest queued **in order**.
  Fixed-step runs apply everything (`apply_budgeted` → `realtime &&
  frame_budget.is_some()`).
- `apply_op` (`:754`) is where a `RepOp` becomes document work: property
  (`store_prop`), name, parent (`set_parent_opts(..., replicate: true)`), tag,
  attribute, destroy, `Moved` (CFrame without `Changed`), persistence.
- `arrive` (`:634`) is the "whose parent hasn't arrived yet" buffer: a child
  whose server parent is not linked is parked (`pending_refs`) and attached when
  the parent shows up. `attach` (`:706`) re-parents and flushes `pending_refs`.
- `update_streaming` (`:433`) / `finish_streaming` handle interest; departures
  are either parked individually (`park`) or, in batches of ≥8, handed to
  `Runtime::stream_out_workspace_parts` (`runtime.rs:8775`).
- `PIECE = 128` (`:98`): big new subtrees are split into pieces of ~128
  instances; `MOVE_BATCH = 32` (`:100`): physics CFrame copies per borrow.

The player drives it per frame from `replicate_with(budgeted)`
(`crates/rustudio-player/src/play.rs:1930`), which takes
`std::mem::take(&mut server_world.rep)` + `stream_requests`, then
`apply_budgeted`/`apply` and finally `rt.dispatch()` (inside a
`bench::scope("rep_dispatch")`). `s.map.begin_frame(dt)` is called at
`play.rs:4182`; `deliver_remotes()` at `play.rs:4787` and `:4935`.
**`World.rep` is only ever pushed on the server side**: `World::record(op)`
(`runtime.rs:859`) starts with `if self.side == Side::Server && self.has_peer`.

**The hook point for M1/M2 is therefore precise and single:** produce
`rustudio_engine::runtime::RepOp`s (`runtime.rs:80`) — or, better, `rbx_types`
values written through the same `Runtime::apply_*` functions `apply_op` calls —
from rove's decoded replication, and feed them into the *same*
`RepMap`-based bounded drain against the client `Document`. No second world, no
second drain loop, no second VM.

`RepOp` (`runtime.rs:80`) is exactly the vocabulary rove's decoder can produce:

```rust
pub enum RepOp {
    Prop(Ref, String), Attr(Ref, String), Tag(Ref, String),
    Name(Ref), Parent(Ref), Moved(Ref), Destroy(Ref), Persistence(Ref, Ref, bool),
}
```

## 4. The mutation API the bridge must target

`crates/rustudio-engine/src/runtime.rs` (the `Runtime` methods that replicate
document changes *and* fire the signals scripts expect). This is the API a
bridge adapter should call rather than writing to `Document` behind the
runtime's back:

| Method | Line | What it does |
| --- | --- | --- |
| `apply_prop(r, name, v: Variant)` | 8667 | store + `mark_prop` + `Changed` (and `HealthChanged`/`Died` for `Health`) |
| `apply_attr(r, name, Option<Variant>)` | 8715 | attribute store + `GetAttributeChangedSignal` |
| `apply_tag(r, tag, present)` | 8726 | `CollectionService` tags + signal |
| `apply_name(r, name)` | 8741 | Name + marks + `Changed` |
| `apply_parent(r, parent)` | 8761 | local parent change |
| `apply_parent_replicated(r, parent)` | 8767 | parent change from replication: `set_parent_opts(..., replicate: true, stop_scripts: true, EnterPose::Replicated)` |
| `notify_prop(r, name)` | 8818 | signal only (value already stored) |
| `notify_parent(r, old, new, descendants)` | 8826 | `ChildAdded`/`ChildRemoved` bookkeeping |
| `apply_destroy(r)` | 8857 | destroy + `AncestryChanged` chain |
| `stream_out_workspace_parts(&[Ref]) -> bool` | 8775 | streaming departure path |
| `finish_invoke(id, ok, args, map)` | 8604 | complete a `RemoteFunction` invoke; `map` maps a `Ref` across the bridge |
| `world_mut()` | 7165 | `AppDataRefMut<World>` — document + queues |

The value-carrying side is `rbx_types::Variant` (the same type
`store_prop_key` (`runtime.rs:3532`) takes), and property names are
`rbx_dom_weak::Ustr`. `set_parent_opts` (`runtime.rs:2911`) is where a parent
change earns its `EnterPose::Replicated`, and `store_prop` (`runtime.rs:3527`)
is the only writer that also marks (`w.dm().mark_prop(r, key)` at
`runtime.rs:3675`, and the CFrame path at `:3792`).

`Xfer` (`runtime.rs:102`) is the cross-VM value vocabulary already implemented
for remotes: `Nil, Bool, Num, Str(Vec<u8>), Table, Inst(Ref), V3, V2, CF, C3,
Brick, UDim, UDim2, EnumItem, Buffer`. Remote routing in M3 must reuse it and
`RemoteMsg`/`RemoteKind` (`runtime.rs:123–140`) rather than inventing a new
envelope; `deliver_remotes` (`play.rs:1997`) already uses `RepMap.c2s` to map a
client `Ref` back to the server's instance.

## 5. Frame, worker thread, and where a network bridge may live

- **Frame:** `PlayerApp::frame()` → asset poll → (if playing) `play_step` +
  `play_tick(ctx, dt, host, no_ui, shift)` (`play.rs:3600`) → `scene.sync` →
  `game.player_viewport()` (`crates/rustudio-player/src/game_view.rs:3695`).
- **Play step:** `step_play(dt, host, keyboard, render_in_parallel)`
  (`play.rs:3675`) runs the physics job (`pending_frame.job.try_join`,
  `job.hurry()` under `physics::hurry_enabled()`), clips dt to
  `mech::MAX_FRAME_DT = 1/15` (`mech.rs:403`) and honours
  `leave_request` / `shutdown_requested` / `StudioTestService:EndTest`.
- **The existing off-thread pattern to copy:** `play_worker.rs`. `PlayWorker`
  (`:13`) owns the `Game` and both Lua VMs for their lifetime on a thread named
  `rustudio-gameplay`; jobs cross as `Box<dyn FnOnce(&mut Game, &mut
  WorkerHost) + Send>` over an `mpsc::channel` (`spawn`, `:19`), each job
  answering on a `sync_channel(1)` (`submit`, `:56`). `PlayView` (`:466`) is
  the published frame data the UI thread reads; `ThreadedPlay` (`:512`)
  is the handle; `EditorEdits::apply` (`:677`) and
  `Publication::capture`/`apply` (`:778`/`:846`) are the two ways state crosses.
  **Lua handles never cross the boundary** (module doc line 1–3).

So a network worker is not a new invention: it is a second `PlayWorker`-style
thread that owns its sockets and rove decoder, and publishes
already-decoded, already-`Variant`-converted ops into a bounded channel that
the gameplay thread drains inside `step_play`, exactly where `replicate_with`
drains `World.rep` today. Rendering, physics and Lua never touch a socket.

**Player creation** (`play.rs:1613` `create_player`) is what binds the identity
the bridge has to attach to:
`Players.Player1` with `domains::players_chat::account_identity`
(`crates/rustudio-engine/src/domains/players_chat.rs:1434`), `Backpack`,
`PlayerGui`, `StarterGear`, the `StarterPack → Backpack` copy, the
`mech::STARTER_PLAYER_TO_PLAYER` (`mech.rs:309`) copies and team auto-assign.
A live join must replace this synthetic player with the server's
`local_player` — same instance shape, `LocalPlayer`/`Character`/`PlayerGui`
refs rebound — not add a second one.

**GUI/CoreScripts:** `ensure_roblox_gui(doc, core_gui)`
(`crates/rustudio-core/src/core_scripts.rs:339`) makes the engine-owned
`CoreGui.RobloxGui` ScreenGui the CoreScripts expect;
`install_cached`/`read`/`decode` (`:148`/`:170`/`:180`) load Roblox's
CoreScripts from the Studio patch, `patch_path()` (`:96`) locates it. M5 must
prefer this over a hand-built HUD.

**Touch input (M4):** `touch_input.rs` `Zones` (`:97`) has `viewport`,
`playing`, `game`, `ui`, `stick`, `jump`, and `Mode` (`:125`);
`game_touch` (`:786`) is the Android path unless `RUSTUDIO_TOUCH` overrides it.
`gui_input.rs::touch_input` (`:1908`) is where fingers become GUI events, with
`sunk`/`began_on` (`:1971`/`:2103`) deciding who keeps a drag. M4 must drive the
existing `PlayerModule` camera/control through these, not a new controller.

**Diagnostics to reuse for the required overlay:** `rustudio_core::diagnostics`
(4 MB cap, 14-day retention, `dir()`/`path()`/`open_folder()` —
`diagnostics.rs:9,10,23,27`), `src/android/logs.rs::export(reason)`
(`:29`, the in-app share bundle), `rustudio_core::bench::scope` (`bench.rs:23`)
and `section_total` (`:382`) for per-section frame costs, plus
`rustudio_core::telemetry` for engine gaps. The overlay demanded by M1–M7
(FPS, avg and 1 %-low frame time, replication ops/frame, queued ops, instance
count, scene rebuild count, script count, last errors, logcat every 5 s,
"Share diagnostics") is an aggregation over these, not new plumbing.

**Hook points that must not be duplicated:** the scene is built from
`rustudio_core::doc` (`crates/rustudio-render/src/scene.rs::sync`) via the
marks of §2; `rustudio-render` and `rustudio-physics` have no `mlua`, no
network, no second world (`render/src/lib.rs` module list, `physics/src/lib.rs`).

---

## 6. What in rove is the network backend

`rove/client-core/src/lib.rs` is the portable half:

```
channels, console, entropy, journal, instance_state, lightweight, memory,
packed_properties, property_storage, physics, property_acknowledgments,
replication, shared_payload, received_store, received_attributes,
received_tags, replication_out, rpc, runtime_delta, runtime_snapshot, schema,
session, terminal, timing, transport (feature), wire, world, windows_private
```

Concretely:

- **Transport.** `client-core/src/transport.rs`: `connect_webtransport_with_delivery`
  (the QUIC/`wtransport` dial, rustls `ring` provider at `:619`, cert-pin
  verification `:834`/`:848`), and `:643`
  `transport.datagram_observer(...)` — the one change rove makes to
  `quinn-proto`, recorded in `rove/vendor/quinn-proto/ROVE-PATCH.md`: an
  opt-in observer on the ACK and loss paths, **no wire or congestion change**.
  Without vendoring that patch the observer calls do not exist.
- **Decoder.** `client-core/src/replication.rs`: `ReplicationDecoder` (`:140`),
  `new(schema)` (`:154`), `globals(payload)` (`:295`), `packet(payload) ->
  PacketTransaction` (`:378`), `commit(&transaction)` (`:571`) — a *transaction*
  model: parse, validate (`validate_globals` `:243`, `validate_commit` `:253`),
  then commit. `sequence()` (`:209`), `client_scope()` (`:212`),
  `global_flags()` (`:239`).
- **World store.** `client-core/src/world.rs:868` `WorldStore`:
  `from_snapshot(&Value)` (`:883`) validating version/`MAX_NODES 400_000`, and
  `apply_delta(&Value)` (`:926`) applying one receipt batch **atomically**
  (`create`/`set`/`remove` only; unknown action kinds are an error rather than a
  silent cursor advance). `pending_sets` holds property sets for instances that
  have not been created yet — rove's own answer to §3's
  `RepMap.pending_refs` ordering problem.
- **Session.** `client-core/src/session.rs` `SessionCore` (`:222`),
  `ingest(payload) -> IngestOutcome` (`:263`),
  `ingest_instance_state(payload)` (`:362`). `SessionDelta` is lazy/eager JSON
  with **no full-world snapshot** — a live join therefore accumulates its world
  from receipts, which is precisely why M1's offline fixture (a snapshot) is a
  separate, simpler path.
- **Schema/values.** `schema.rs` (class and property ids), `wire.rs`
  (`Cursor`-style reader, `:42` `new(&[u8])`), `shared_payload.rs`,
  `packed_properties.rs`, `lightweight.rs`, `timing.rs`.

`rove/rust-client/src/live_session.rs` is the session owner worth porting
wholesale (minus its VM): `run_with_options(issued, seconds, config,
physics_enabled)` (`:1752`) builds the allocation, 20 s connect timeout
(`tokio::time::timeout(Duration::from_secs(20), connect_webtransport_with_delivery…)`),
checks `capability_negotiation()` and requires `BYTECODE_RANGE_CAPABILITY =
0x10_0000` (`:57`, enforced `:1788`), runs a `current_thread` tokio runtime,
`start_receivers` (`:1679`), a 20 ms tick interval (`:1870`,
`MissedTickBehavior::Skip`), and produces `enum Effect` (`:92`):
`Pings, Reply, PropertyAck, StreamingReady, Focus, RpcOutbound, OwnedParts,
Failed`. That effect enum *is* the bridge's input queue — one enum, one
consumer.

## 7. The gamejoin path that must be used (not the old one)

`rove/rust-client/src/allocation.rs` at `70b24c3`:

- Endpoint: `UdmuxEndpoints[0].Address` if present (`:891`), else
  `MachineAddress` (`:905`) — **no udmux relay framing**, the current
  gamejoin shape.
- `NetStackConfig` (`:911`) is the URL-encoded JSON that carries
  `port` (`:927`) and `certHashes` (`:933`, base64, `MAX_CERT_PINS ≤ 16`).
- `NetStackTokenValue` (`:691`): absent or empty → zero token; malformed
  base64 → rejected.
- `validate()` requires IPv4, non-zero port, pins, and
  `capabilities ∈ {0, 0x10_0000}`.
- `IssuedJoinAllocation` is at `:432`.

Login wire codec is `rove/rust-client/src/login.rs` (LEB128 unsigned/zigzag,
`VERSION = 36`, the bytecode range `[6,9,13,14]` accepted); it is portable
as-is and does **not** answer integrity challenges — that is `session_integrity.rs`
and is not part of this client's scope.

The older gamejoin/udmux `allocation.rs`/`transport.rs` must **not** be copied:
the pinned commit replaced them.

## 8. What must not be taken from rove, and what was removed

Non-negotiable rule 3 (one VM) and the explicit "do not" list mean the
following are *reference only, not portable*: `rust_client/host`-style
`rust_host/` (its own VM owner thread `vm_loop`, its script catalog executor,
Windows-only bits), `bin/rove-exec`, `decompiler.rs`, `ui_server.rs`,
`session_ui.rs`, `dashboard.rs`, `join_cli.rs`, `engine_bridge.rs` (the legacy
C++ callback tables), `terminal`, `ui/`, `runtime/`, `stellar-sandbox/`.

They are not in this tree: the mirror removed exactly those, and
`rove/README-MIRROR.md` lists every removal with its reason. The pruned
`rove/rust-client/src/lib.rs` keeps the modules the port needs (`allocation`,
`live_session`, `live_runtime`, `login`, `movement_math`, `isr_sender`,
`physics_*`, `program_check`, `script_catalog`, `runtime_sources`, …) and no
`rust_host`.

Two further consequences worth stating now, because they shape M3:

- `program_check.rs` / `PublicTrust`/`ProgramAuth` in `run_with_options`
  (`:1765`, `:1816`) verify server-sent bytecode against pinned keys, and
  `script_catalog.rs` normalizes to bytecode 13/14 type_version 3 with
  `MAX_CODE 128 KiB`, `MAX_CATALOG 16 MiB`, `MAX_CATALOG_ENTRIES 16384`
  (`runtime_sources.rs:184–205`). rustudio must accept the *same catalog shape*
  and run it in **its own** Luau (`rustudio-engine`'s `mlua` runtime) — the
  admission/eligibility and new-allocation logic is what gets ported, not the VM.
- `runtime_sources::load_class_parents` reads
  `runtime/class-parents-745.json`, which is rove's 0.745 class hierarchy. This
  repo already has a newer equivalent: `api-dump.json` (955 classes, `Version
  1`, members with `Name/MemberType/Security/Serialization/ThreadSafety/
  Category/ValueType{Category,Name}/Capabilities/SimulationAccess`). The ported
  loader must read **our** `api-dump.json`; rove's table is kept at
  `rove/runtime/class-parents-745.json` (the `include_str!` its decoders use)
  for comparison only.

## 9. Value boundary: rove JSON → `rbx_types::Variant`

rove's world JSON is `$`-tagged: `$vector3`, `$cframe`, `$ref`, `$enum`,
`$udim`, `$udim2`, `$color3`, `$rect`, `$brickcolor`, `$number_range`,
`$color_sequence`, `$bytes_hex`, `$shared_string`, `$number`, `$datetime`,
`$faces`. `rustudio_core::project_values` already owns both directions of exactly this
conversion for place files: `resolve(class, prop, &J)` (`:324`) →
`(canonical name, Variant)`, with `descriptor` (`:27`) reading the reflection
database, `variant_type` (`:279`) the JSON type names
(`Bool/String/Float32/Float64/Int32/Int64/Vector3/Vector2/Vector3int16/
Vector2int16/CFrame/OptionalCFrame/Color3/Color3uint8/UDim/UDim2/NumberRange/
NumberSequence/ColorSequence/Rect/Content/ContentId/BrickColor/Tags/
Attributes/Font/Faces/Axes/PhysicalProperties`), `explicit` (`:314`) the
`{"Enum": n}` / `{"Vector3": …}` form, `typed` (`:123`) the value walk and
`to_json` (`:375`) the inverse (`Variant → JSON`). rove's tags map onto this
vocabulary (`$vector3`→`Vector3`, `$cframe`→`CFrame`, `$udim`/`$udim2`→`…`,
`$color3`→`Color3`, `$rect`→`Rect`, `$brickcolor`→`BrickColor`,
`$number_range`→`NumberRange`, `$color_sequence`→`ColorSequence`,
`$faces`→`Faces`, `$enum`→`{"Enum": n}`, `$number`→`Float64`, `$datetime`→
`Variant::DateTime`, `$shared_string`/`$bytes_hex`→`String`/binary,
`$ref`→`Variant::Ref`), so the bridge is a translation into *that* function and
does not need a new value decoder. `$ref` is the one case that must **not** go
through `resolve`: it needs the rove-id→`Ref` map, and a `Ref` whose target has
not arrived is parked (§3 `pending_refs`).

The bridge converts **once, at the boundary**, into `rbx_types::Variant` —
never JSON in the frame path. `$ref` becomes `rbx_types::Ref` resolved through
the `RepMap`-equivalent id map (rove id → `Ref`), and child-before-parent
ordering is handled by the same park-and-attach rule as `replicate::arrive`
(`replicate.rs:634`), not by a second mechanism.

Fixture for M1: `rove/tests/fixtures/world_snapshot.json` is small and
synthetic — 1.6 KB, `version 1`, `sequence 7`, `job_id
"synthetic-snapshot-job"`, `local_player "scope:player-1"`, services
`Workspace/Players/RunService`, a `class_parents` table, and 7 nodes:
`Workspace(Gravity 196.2)`, `Players(LocalPlayer → player-1)`,
`RunService`, `Player "FixturePlayer"` (UserId 123, `Character → character-1`),
`Model "FixtureCharacter"`, `Humanoid` (Health 100, WalkSpeed 16),
`Part "HumanoidRootPart"` (Anchored, `$vector3` position `[1,2,3]`,
`$cframe` identity). That is enough to prove the whole chain end to end:
IDs → `Ref`s, `$vector3`/`$cframe` → `Variant`, `LocalPlayer`/`Character`
binding, parenting under `Workspace`, and a rendered part — which is exactly
M1's acceptance. It is *not* enough to prove scale; `WorldStore::from_snapshot`
caps at 400 000 nodes and M6's reference sizes are 1818 ≈ 5.6 k nodes,
BedWars ≈ 3.4 k nodes / ~2.5 k scripts, Phantom Forces ≈ 89 k nodes.

## 10. What this fixes for the milestones

**10.1 M1 (offline bridge) — shape.** A `rove_snapshot` reader in a new crate
(working name `crates/rustudio-live`): `WorldStore::from_snapshot` →
class-name→class and `$`-tag→`Variant` conversion (through `api-dump.json` for
classes, `project_values` conventions for values) → build the client
`Document` the way `initial_client` does → hand it to `rustudio-player`.
Acceptance needs no network at all, and the harness already prints numbers:
`crates/rustudio-player/src/run_harness.rs` `--screenshot`, `--result FILE`
(one JSON of frame statistics, `:60`), `shot_after` (`:12`),
`tick_before_shot` (`:312`).

**10.2 M1 — `mark_all` budget.** `mark_all()` exactly once, after the initial
document is built and before the first `scene.sync`. Everything after that:
`mark_prop`/`mark_tree` through the `apply_*` API of §4.

**10.3 M2 (join 1818) — threading.** One network thread owning tokio, the QUIC
connection and rove's decoder; it converts effects (`live_session::Effect`) and
replication into ops carrying `Variant`s, and publishes them on a **bounded**
channel (the `PlayWorker`/`sync_channel` pattern, `play_worker.rs:56-60`). The
gameplay thread drains under the existing 2 ms budget in `step_play`
(`play.rs:3675`, budget from `RepMap::begin_frame`, `replicate.rs:168`), so
`World.rep` is replaced as a *source*, never accompanied by a second drain.
Lightweight/live mode (`client-core/src/lightweight.rs`) is the switch that keeps
non-visual properties off the wire; `stream_out_workspace_parts`
(`runtime.rs:8775`) covers departures in ≥8 batches.

**10.4 M2 — player binding.** The bridge must reuse `create_player`'s shape
(`play.rs:1613`) but bind to the server's `local_player` node: `LocalPlayer`,
`Character`, `PlayerGui` refs, and the `Players.PlayerAdded`-before-character
ordering that `replicate::arrive` enforces. No second player.

**10.5 M3 — remotes and scripts.** `RemoteMsg`/`RemoteKind`/`Xfer`
(`runtime.rs:123–140`), `deliver_remotes` (`play.rs:1997`) and
`RepMap.held`/`c2s` already implement ordered client↔server remotes with `Ref`
translation; the network side maps them onto rove's `rpc` module
(`client-core/src/rpc.rs`: `RpcSession`, `ServerCall`, `ServerEvent`,
`ClientReply`, `RpcOutboundKind`). Scripts: rove's catalog shape and
`ProgramAuth`/`PublicTrust` admission, executed in rustudio's Luau; the
in-app log requirement is `src/android/logs.rs::export` plus a `ScriptContext`
log line — the `[Client]` tag in `with_side` (`play.rs:764`) already exists.

**10.6 M4 — character.** rustudio's own `PlayerModule` ControlModule/CameraModule
(via `default_scripts`) driven by `touch_input.rs` zones, with movement sent as
rove's own movement records (`movement_math.rs`, `isr_sender.rs`, `physics_sender.rs`,
`owned_part_*`), and server ownership honoured through
`replicate::update_streaming`/`RepMap.s2c`. No custom controller.

**10.7 M5 — chrome.** `ensure_roblox_gui` + `core_scripts::install_cached`
first; any fallback HUD must be documented as such. `CoreGui`/`PlayerGui` are
bound during the join (§10.4), and chat (legacy + `TextChatService`)
goes through whatever the installed CoreScripts do.

**10.8 M6/M7 — measurement before more code.** `docs/ENGINE-GAPS.md` already
defines the gap kinds (`missing`, `call-unimplemented`, `default-only`,
`write-ignored`, `event-unwired`) fed by `RUSTUDIO_GAPS` telemetry; M6 logs
Legends Battlegrounds' gaps in those terms with `api-dump.json` citations.
M7's APK-size and GPU work starts from `docs/ANDROID.md` (Turnip/PanVK,
`ANDROID_SIGNING_PEM`, `x build`), and the CI to build it is described next.
One measurement is already done, because it decides which APK is worth
installing: on this tree a **debug** APK is **314 MB** (aarch64) / **322 MB**
(x86_64) and about 1 GB installed, against a **release** APK's **39 MB** /
**30 MB** — so `android.yml` now builds release only (debug on request), and a
push that changes `src/`, `crates/`, `vendor/` or `Cargo.*` starts an APK build
while a `rove/`- or `docs/`-only push does not.

**10.9 CI, as it stands in this tree.** Two workflows: `.github/workflows/ci.yml`
(desktop `check` on ubuntu+windows, tests on macOS only — the Linux runner has no
display, so `cargo test` is skipped there) and `.github/workflows/android.yml`
(`apk` matrix arm64/x64 × debug/release via xbuild; `desktop-still-builds`;
`release` on `v*` tags). **Neither has a gamescope headless job**, which M1
needs for a screenshot. M1 therefore adds one job to `ci.yml` (or a new
`harness.yml`): install gamescope + Mesa software Vulkan, run
`rustudio --no-ui --headless … --screenshot … --result out/result.json`, then
`cat` the result and list the screenshot before uploading it — printing numbers
into the log is mandatory because artifacts cannot be downloaded back.

**A correction to "tests on macOS only", from what that turned out to cost.**
The macOS suite has failed on every run of this tree: `ui_literals_use_bundled_glyphs`
reports a UI string no bundled font can draw, and because the test is where the
suite runs, the failure appeared a queue-length after the push, on the slowest
runner, in a job that also had to build and link everything. It never once
acted as a gate — the same test was red while the string it named was still in
the tree.

The rule it enforces is a property of the *source*, so `check` applies it now,
on ubuntu and windows, in three minutes: `tools/ci/ui_glyphs_preflight.py`
(same scan, no toolchain — the only test-shaped check the Linux runner can run
without a display) followed by `cargo test -p rustudio --test ui_glyphs`
itself, so the cheap scan and the authority run side by side and a disagreement
is visible in the fast job. The macOS suite keeps running the whole thing; it
is no longer the only place this can be found.

On the remote, workflow id `431220484` is `Android` at
`.github/workflows/android.yml` and is *active*, although `main`'s tree at
`70b24c3` has no such file: Git registers a workflow the first time it appears
on **any** branch and keeps it active. So it was registered by the deleted
`arena/4baa9b30-rove-android` branch's file, and this mirror (which brings
`android.yml` back, from rustudio) puts it back in the tree — the next push
starts APK jobs for arm64 and x64, debug and release. Its `paths:` filter lists
`src/**`, `crates/**`, `vendor/**`, `Cargo.*`, `api-dump.json`,
`mechanics.json`, `tools/android/**` — but **not** `rove/**`, so a rove-only
commit will not start an APK build. That is correct behaviour, not an accident
to fix.

---

### Open questions for acceptance

1. **M1 fixture scope.** `world_snapshot.json` (7 nodes) proves the chain but not
   scale. Do you want M1 to *also* render a larger captured world (a real 1818
   region) from the same offline path before M2, or is the fixture enough and
   scale is proven at M2?
2. **Repo layout.** rustudio is now the whole tree and rove is `rove/`
   (non-workspace, reference + port source). If you would rather rove stay a
   *separate* clone and this repo hold only the client, say so before M1 — the
   move is cheap now and expensive later.
3. **Harness vs app for M1.** M1 will render through `rustudio-player` under
   gamescope in CI (per your decision). The Android app's own first milestone
   view will then be M2. Confirm that ordering is what you want.
