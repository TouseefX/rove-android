# Rove ↔ Rustudio Live Client Bridge & M3 Architecture Guide

**Target Repository:** `TouseefX/rove-android` (branch `arena/09b28e1f-rove-android`, audited at HEAD `38e42f83f0`)  
**Reference Client:** `rove` (`alexkkork/rove` mirror vendored in `rove/`, pinned at `3c375c274696`)  
**Protocol Cross-Reference:** `kingdudely/Boblox` (decompiled client/server RakNet & engine semantics)  
**Scope:** Pure client-only live Roblox gameplay on Android (**M3**: continuous replicated world, genuine server-trusted session, local character spawn + camera + controls, bidirectional physics/ISR/RPC).  
**Non-Goals / Hard Constraints:** No local server (`RunMode::Play`) fallback, no synthetic local character spawn, no desktop/ADB instructions, no executor shell or challenge-bypass stubs.

---

## Table of Contents

1. [Executive Diagnosis: Why `38e42f83f0` Behaves "Same as Before"](#1-executive-diagnosis-why-38e42f83f0-behaves-same-as-before)
2. [Protocol Comparison: `rove`/`rustudio` (WebTransport/QUIC) vs `Boblox` (RakNet)](#2-protocol-comparison-roverustudio-webtransportquic-vs-boblox-raknet)
3. [Gap 1 (Critical): Missing `RuntimeDeltaNormalizer` — Why `Workspace` Has Parts with Zero `CFrame` and Zero `Size`](#3-gap-1-critical-missing-runtimedeltanormalizer--why-workspace-has-parts-with-zero-cframe-and-zero-size)
4. [Gap 2 (Critical): Unanswered Packet `155` (`0x9B`) Signed-Program Challenge](#4-gap-2-critical-unanswered-packet-155-0x9b-signed-program-challenge)
5. [Gap 3 (Critical): Broken Ping Loop — Post-Globals Pings Ignored, Wrong Reply Words, Wrong Route](#5-gap-3-critical-broken-ping-loop--post-globals-pings-ignored-wrong-reply-words-wrong-route)
6. [Gap 4 (Critical): `flush_outbound` Misclassifies Every `0x83` Packet as `PropertyAck`](#6-gap-4-critical-flush_outbound-misclassifies-every-0x83-packet-as-propertyack)
7. [Gap 5: Marker Queue Readiness, Streaming Resume, Focus Loop, and `character_present()`](#7-gap-5-marker-queue-readiness-streaming-resume-focus-loop-and-character_present)
8. [Gap 6: Client-to-Server Gameplay Uplinks (`PhysicsSender`, `IsrSender`/`IsrClock`, `RpcSession`, `TouchContact`)](#8-gap-6-client-to-server-gameplay-uplinks-physicssender-isrsenderisrclock-rpcsession-touchcontact)
9. [Engine, Ownership, and Rendering Bridge Details (`rustudio-core`, `rustudio-live`, `rustudio-player`, `rustudio-render`, `src/`)](#9-engine-ownership-and-rendering-bridge-details)
10. [Boblox Protocol & Engine Cross-Check (Transfers vs Non-Transfers)](#10-boblox-protocol--engine-cross-check-transfers-vs-non-transfers)
11. [Ordered Implementation Plan for a Coding Agent](#11-ordered-implementation-plan-for-a-coding-agent)
12. [Verification & Telemetry Acceptance Criteria (Android In-App Report)](#12-verification--telemetry-acceptance-criteria-android-in-app-report)

---

## 1. Executive Diagnosis: Why `38e42f83f0` Behaves "Same as Before"

Commits `00976d2..38e42f8` wired up `CONTROL_CHANNEL` datagram routing, `PropertyAcknowledgments`, `LiveWorld::set_focus`, `155` counting, banner replacement for `join_cover`, `claim_streamed_parts`, and `Workspace` diagnostics. However, inspecting the full codebase at `38e42f83f0` against `rove/` and the `db7aaf1a7b` device logs reveals **five interlocking root-cause failures** that keep the live session blind, untrusted, frozen, or silent:

| # | Root Cause | Location in `rove-android` | Symptom Seen on Device |
|---|---|---|---|
| **1** | **`RuntimeDeltaNormalizer` is never called.** `Session::drain_batch` reads raw `row.value.world_delta["actions"]` instead of normalizing through `rove_client_core::runtime_delta::RuntimeDeltaNormalizer`. | `crates/rustudio-join/src/session.rs:1163`, `crates/rustudio-live/src/snapshot.rs` | `Workspace 954` instances exist in the DOM, `Part.NetworkOwnerV3 ×10` writes, yet **`Part.CFrame` = 0 and `Part.Size` = 0** because wire `ReplicationPV` (type `57`), wire `CFrame` (type `27`/`51`), and lowercase `size` (type `22`) are never expanded/normalized before `Live::apply`. No map geometry can render or collide at real positions. |
| **2** | **Packet `155` (`0x9B`) challenges are only counted, never answered.** `rove/inputs/bytecode-signature-v1-public.{bin,json}`, `program_check.rs`, and `bytecode_format.rs` exist in `rove/` (and `vendor/luau0-src` already supports bytecode v3–14), yet `session.rs:511-518` logs a warning and returns `false`. | `crates/rustudio-join/src/session.rs:511-518` | Live Roblox servers require a valid `[155, u2, answer]` reply on `(4, 1)` to trust the session; unanswered `155` challenges cause the server to withhold local character spawn and full world streaming. |
| **3** | **All post-globals pings are silently dropped; pre-globals pings send wrong words and wrong transport route.** After `GlobalsCommitted`, `(4, 3, 131)` goes to `core.ingest`, which stores pings in `row.value.pings`, and `drain_batch()` never reads `row.value.pings`. Pre-globals replies use invented DOM counters instead of `PingClock` + `SessionPingTelemetry`, and never use unreliable datagram `PING_WIRE_ID = 6`. | `crates/rustudio-join/src/session.rs:495-501`, `902-943`, `1160-1244` | Server loses continuous replication heartbeat/RTT telemetry after globals and treats the client as unresponsive or unhealthy. |
| **4** | **`flush_outbound` treats every `0x83` packet on `(4, 1)` as a `PropertyAck`.** `reply.first() == Some(&0x83)` matches `streaming_ready` (`[131, 3, ...]`), `marker_reply` (`[131, 4, ...]`), and `replication_focus` (`[131, 26, ...]`) because `0x83 == 131`. | `crates/rustudio-join/src/session.rs:1809-1833` | `property_acks_sent` increments falsely and `property_ack_written(true)` commits/corrupts `ack_in_flight` for unrelated control messages. |
| **5** | **Missing editor-to-session uplinks (`set_focus`, `PhysicsSender`, `IsrSender`, `RpcSession`) + `character_present` bug.** Nobody in `src/` or `rustudio-player` calls `live_world.set_focus(...)`; `character_present()` checks *any* player instead of `account_user_id`; and there is no character root upload (`(4, 6)`), owned-part ISR upload (`(4, 5)`), or `RemoteEvent`/`RemoteFunction`/`TouchContact` (`RpcSession`) bridge. | `crates/rustudio-join/src/session.rs:843-855`, `src/join_window.rs`, `crates/rustudio-player/src/play.rs` | Even once a character spawns, walking does not replicate to the server, loose owned parts never sync, UI/game remotes cannot fire, and `streaming_ready` can misfire on other players' characters. |

Fixing **Gaps 1–4** unlocks server trust, continuous heartbeats, real `Workspace` geometry (`CFrame`/`Size`/`ReplicationPV`), and local character spawning. Fixing **Gaps 5–6** unlocks camera-driven streaming, walking on the server, interactive remotes (`RemoteEvent`/`RemoteFunction`), and continuous live gameplay.

---

## 2. Protocol Comparison: `rove`/`rustudio` (WebTransport/QUIC) vs `Boblox` (RakNet)

Do **not** mix wire opcodes between `kingdudely/Boblox` and `rove`/`rustudio`:

| Layer | `kingdudely/Boblox` (C++17 / Python) | `rove` & `rustudio` (Rust) | Bridge Action |
|---|---|---|---|
| **Transport** | RakNet UDP (`0x83` data wrapper, `0xA1` ISR, `0xA6` ping, `0x9B` challenge) | WebTransport / QUIC (`quinn` + `wtransport`) with RUPP framing when edge requires (`rove/client-core/src/transport/`, `channels.rs`) | Keep `rove-client-core`'s QUIC/WebTransport + RUPP transport exclusively. |
| **Reliable Streams & Wire IDs** | RakNet reliability channels | Framed QUIC streams (`6, 1, app, chan_u32_be`), unreliable datagrams announced via `CONTROL_CHANNEL` (`[2, app, chan_u32_be, wire_u32_be]`) | Keep `session.rs` `CONTROL_CHANNEL` map + add client-side `control_open` announcements for outgoing unreliable channels (`PING_WIRE_ID = 6`, `PHYSICS_WIRE_ID = 8`, `ISR_WIRE_ID = 10`). |
| **Replication & Schema** | Custom RakNet `0x83` sub-packets (`0x05` ISR, `0x06` physics, `0x07` events, `0x0a` acks) | QUIC `151` (Schema), `129` (Globals), `131` (Replication + sub-opcodes), `132` (Marker), `147` (Feature table), `155` (Signed program), `161` (ISR), `134` (Contacts) | Use `rove_client_core::SessionCore` + `RuntimeDeltaNormalizer` + `PingResponder` + `ProgramAuth` + `RpcSession`. |
| **Engine / Ownership / Place Semantics** | Decompiled `libroblox.so` (`NetworkOwnerJob::updatePlayerLocations`, `ReplicationFocus`, `UserId` 64-bit, `0x9B` bytecode semantics, Place `1818` vs `189707`) | Identical Roblox RCC server & `0.745`/`0.747` engine rules | Transfer all engine semantics directly (Sections 4, 9, 10). |

### Exact Channel & Wire-ID Map (from `rove/rust-client/src/live_session.rs` & `live_ping.rs` & `physics_sender.rs` & `isr_sender.rs`)

| Direction | App | Channel | Kind / Wire ID | Purpose |
|---|---|---|---|---|
| C ↔ S | `0, 4, 6` | `CONTROL_CHANNEL` (`0x6374726c`? / `channels::CONTROL_CHANNEL`) | `[0, 0]` open; `[2, app, chan, wire]` | Announce unreliable `wire_id → (app, chan)` mappings |
| C → S | `6` | `1` (reliable) | opened at connect | Clock reliable channel |
| C ↔ S | `6` | `2`, `3` (unreliable) | client announces `wire 2` (`6/2`), `wire 4` (`6/3`) | `SessionWireClock` & `TimeSyncProbe` |
| C → S | `4` | `1` (reliable) | `0xa8, 0x90, 0x92, 0x8a, 0x8f` | 5-packet login sequence |
| S → C | `4` | `1` (reliable/datagram) | `151` Schema, `147` Features, `131` Rep, `132` Marker, `155` Program Auth | Core world replication & auth |
| S → C | `4` | `2` (reliable) | `129` Globals | `GlobalsCommitted` (`client_scope`, `Players`, etc.) |
| C → S | `4` | `1` (reliable) | `[155, u2, ans]` | Signed-program challenge reply |
| C → S | `4` | `1` (reliable) | `[131, 4, marker[1..5], 0]` | Marker ack (gated on `client_scope && tag 13`) |
| C → S | `4` | `1` (reliable) | `[131, 3, player_guid, prop_id, 0, 0, 0]` | `streaming_ready` (`GameplayPaused = false`) |
| C → S | `4` | `1` (reliable) | `[131, 26, ...]` | `ReplicationFocus` updates |
| C → S | `4` | `1` (reliable) | `[0x83, ...]` (`PropertyAcknowledgments`) | Versioned `CFrame` property acknowledgments |
| S → C | `4` | `3` (reliable or datagram) | `131` Ping (pre-globals direct; post-globals via `record.pings`) | Server ping requests & responses |
| C → S | `4` | `3` (reliable or `PING_WIRE_ID = 6` datagram) | `[0x83, 6, 1, ...]` | `PingResponder` replies (routed per `item.unreliable`) |
| S → C | `4` | `5` (reliable or datagram) | `161` (`0xa1`) | Server Instance State Replication (ISR) |
| C → S | `4` | `5` (`ISR_WIRE_ID = 10` datagram) | `161` (`0xa1`) | Client-owned assembly poses (`IsrSender`) |
| C → S | `4` | `6` (`PHYSICS_WIRE_ID = 8` datagram) | `0x1b` | Client-owned character root frame (`PhysicsSender`) |
| C → S | `4` | `7` (reliable) | `134` (`0x86`) | `contact_batch` / `contact_step_batch` (`TouchContact`) |
| C ↔ S | `4` | `1` / unreliable | `RpcSession` (`RemoteEvent` / `RemoteFunction`) | Script RPC events & invocations |

> **Important Client Wire-ID Collision Rule:** Client-allocated outgoing `wire_id`s (`2` for `6/2`, `4` for `6/3`, `6` for `4/3`, `8` for `4/6`, `10` for `4/5`) are *even* and local to client→server datagrams, separate from server→client `wire_id`s stored in `session.channels` (which servers allocate as odd numbers like `3, 5, 7, 9`). Keep server-announced `channels` strictly for *incoming* datagrams; track outgoing `unreliable_announced` booleans per outgoing writer (`PingWriter`, `PhysicsWriter`, `IsrWriter`).

---

## 3. Gap 1 (Critical): Missing `RuntimeDeltaNormalizer` — Why `Workspace` Has Parts with Zero `CFrame` and Zero `Size`

### What Happens Today in `rove-android`

1. In `crates/rustudio-join/src/session.rs:1163`:
   ```rust
   let actions = row.value.world_delta["actions"]
       .as_array()
       .cloned()
       .unwrap_or_default();
   ```
   `Session::drain_batch` reads `row.value.world_delta["actions"]` **directly** from `SessionCore`.
2. In `rove`, `SessionCore` emits raw decoded wire shapes inside `world_delta` (for example, `ReplicationPV` as type `57`, `CFrame` as `{"position": [...], "rotation_id": 2}` without `"$cframe"`, and `BasePart.Size` under wire property name `"size"`).
3. `rove` transforms every `SessionRecord` through `rove_client_core::runtime_delta::RuntimeDeltaNormalizer` (`rove/client-core/src/runtime_delta.rs:49-450`) before handing actions to the VM/DOM consumer:
   - `normalize_set_fields` (`runtime_delta.rs:902-948`):
     - For property types `27`, `51`, or `57`, calls `normalize_cframe(value, type_id)` which converts identity `rotation_id == 2` (for types `27`/`51`) or `rotation == 1` (for type `57`) into `{"$cframe": { ..., "rotation": [1,0,0,0,1,0,0,0,1], "rotation_mapping": "native_745_identity" }}`.
     - If `ReplicationPV` (type `57`) is present: copies normalized `ReplicationPV` into `"CFrame"` if `"CFrame"` isn't already present, and extracts `AssemblyLinearVelocity`, `Velocity`, `AssemblyAngularVelocity`, `RotVelocity` via `runtime_snapshot::received_velocity(&pv, key)`.
     - If `"Size"` is absent and `"size"` (type `22`) is present: copies `"size"` → `"Size"`.
   - `normalize_create_node` (`runtime_delta.rs:818-900`):
     - Normalizes `$bytes_hex` `Name`, `$ref` `parent` strings, top-level service roots, `normalize_node_values(node, types)` (same `CFrame`, `ReplicationPV`, and `size` → `Size` fixes on `create` actions!), `AttributesReplicate`, and `Tags`.

### Why `db7aaf1a7b` Showed `Workspace 954` with Zero `Part.CFrame` and Zero `Part.Size`

- In `logcat-srustudio (2).txt`, `join world: 14 properties written` listed **only** Ref properties (`Motor6D.Part0`, `Part.NetworkOwnerV3`, `Player.Character`, etc.) and **never once** `Part.CFrame` or `Part.Size`!
- Why? Two reasons working together:
  1. In `set` actions: wire `size` was stored or refused under `"size"` (not `"Size"`), `ReplicationPV` arrived under `"ReplicationPV"` (never copied to `"CFrame"`), and `CFrame` without `$cframe` depended on fallback parsing.
  2. In `create` actions: `Live::create` (`crates/rustudio-live/src/live.rs:459-485`) does **not** record properties in `self.written_properties`, AND `Live::create` calls `self.convert(property, class, value, conv)` directly on un-normalized `node["properties"]` where `"size"` (lowercase) is not `"Size"` in `api-dump.json` and `"ReplicationPV"` is not `"CFrame"`!

### Exact Fix

1. **Instantiate `RuntimeDeltaNormalizer` inside `Session` once `Schema` is installed** (`IngestOutcome::SchemaInstalled` or first `self.core.schema()` availability).
   - Also keep `pre_receipt_view` / `WorldView`: `RuntimeDeltaNormalizer::normalize(&pre_receipt_view, &record, initial_world_tag_received)` only uses `pre_receipt_view` to look up existing node `class_name` for `set` actions (`runtime_delta.rs:1-4`).
   - Note on `Session::core` read timing: `self.core.world_view()` returns the **post-receipt** view after `core.ingest` committed. Check `runtime_delta.rs:251-258`:
     ```rust
     if actions.first().and_then(|a| a.get("sequence")).and_then(Value::as_u64)
         .is_some_and(|sequence| sequence <= base_sequence)
     ```
     Because `normalize_record` checks `pre_receipt_view` sequence (`base_sequence < first_action_sequence`), you should either:
     - Capture `let pre_view = self.core.world_view();` **before** calling `self.core.ingest(payload)` / `self.core.ingest_instance_state(payload)` (just like `rove/rust-client/src/live_session.rs:378`: `let previous = self.core.world_view();`!), OR
     - If multiple packets were ingested before `drain_batch()`, normalize **immediately after each `ingest` / `ingest_instance_state` commit** (matching `live_session.rs:378-493` one-packet-at-a-time receipt read + normalize + acknowledge!).
   - **Recommended architecture:** Match `rove/rust-client/src/live_session.rs:378-493` directly:
     1. Capture `let previous = self.core.world_view();` before `core.ingest` / `core.ingest_instance_state`.
     2. On `GlobalsCommitted` or `ReplicationCommitted { packet_sequence, .. }`, read that exact record (`read_after(self.reader, self.acknowledged, 1)`), observe `tags`/`pings`/`regions`/`events`, advance `PropertyAcknowledgments`, call `normalizer.normalize(&pre_view, &entry.value, self.initial_tag)` (for `GlobalsCommitted` where `pre_view` might be empty/None before globals, initialize a `WorldStore::new().view()` or call `normalize_for_shared_view` / `bind_snapshot_services` appropriately — verify `runtime_delta.rs:160-260`!).
     3. Feed `normalized.worker_delta["actions"]` (NOT raw `row.value.world_delta["actions"]`!) into `self.live.apply(...)` AND `handover.publish(...)`.
     4. Acknowledge `self.core.acknowledge(self.reader, sequence)` immediately.
2. **Belt-and-suspenders check in `crates/rustudio-live/src/live.rs`:**
   - Also check `Live::create` (`live.rs:479-484`): record initial `CFrame`/`Size`/`MeshId`/`Color` stats or ensure `workspace_summary(doc)` in `src/join_window.rs:83-114` counts actual `BasePart`s with valid `CFrame` and non-zero `Size` after `create` + `set`!
   - Also verify `crates/rustudio-live/src/snapshot.rs` handles `"ReplicationPV"` gracefully if ever encountered, and verify `via_place_value` / `cframe::frame` handles every `rotation_id` (`0..=2` and quaternion/matrix forms) produced by `runtime_delta::normalize_cframe`. Wait: check `crates/rustudio-live/src/cframe.rs` against `rove/client-core/src/runtime_delta.rs:950-980` and `rove/client-core/src/replication.rs` so non-identity `rotation_id` (e.g. `rotation_id` != 2, explicit `rotation` 9-float or 4-float quaternion) converts identically!

---

## 4. Gap 2 (Critical): Unanswered Packet `155` (`0x9B`) Signed-Program Challenge

### What `rove` Does (`rove/rust-client/src/program_check.rs` & `live_session.rs`)

When Roblox sends packet `155` (`0x9B`) on `(4, 1)`, `rove` executes:
1. **`PublicTrust` initialization** (`program_check.rs:82-139`):
   - Loads `rove/inputs/bytecode-signature-v1-public.bin` (99,084 bytes = `8257 * 12` bytes, SHA-256 `0b6cdf37e5817b29638fe705319618df725ffd2aafc0491d768ab6f162f42c3c`) and `rove/inputs/bytecode-signature-v1-public.json` (SHA-256 provenance check).
   - **On Android**, do **not** load these from disk relative paths! Embed both via `include_bytes!("../../../rove/inputs/bytecode-signature-v1-public.bin")` and `include_str!("../../../rove/inputs/bytecode-signature-v1-public.json")` and call `PublicTrust::from_pinned_bytes(raw, &provenance)`.
2. **`ProgramAuth::for_allocation(&issued, trust)`** (`program_check.rs:193-224`):
   - Requires `issued.game_id` (36-char UUID `JobId`).
   - Maintains a bounded duplicate cache (`max 16` SHA-256 keys).
3. **`ProgramCheck::parse(packet)`** (`program_check.rs:33-69`):
   - Wire header: `[155][first: u32le][second: u32le][length: u32le][blob: length bytes]`.
   - `arguments() == [second, first]` (note the reversed order: `second` is arg 1, `first` is arg 2!).
4. **`unpack_container_observed` + `PublicTrust::verify`** (`program_check.rs:140-175, 309-343`):
   - Decodes the `RSB1` container (`key[i] = (blob[i] ^ b"RSB1"[i]).wrapping_sub((i * 41) as u8)`, XOR-decrypts, checks `ticket_hash(&raw, 42) == u32::from_le_bytes(key)`, zstd-decompresses `[8..]` when `expanded > 0`).
   - Verifies the 24-byte V1 signature trailer (`[0x32, 0xc4, 0x6a, 1]`, tag `0x74171870`, `evaluate_public_map` == `blake3(body)[4..16]`).
   - Only if signature verification passes does it return `VerifiedProgram`.
5. **Bytecode normalization + VM execution**:
   - The verified bytecode body is `&verified.original_bytes()[..len - 24]`.
   - **Critical detail from `rove/rust-client/src/bytecode_format.rs:548-577`:** Roblox's signed bytecode encodes instruction opcodes with an invertible odd byte multiplier (e.g. `227` — `op.wrapping_mul(227)`). `bytecode_format::normalize(body, None)` parses bytecode versions `6 | 9 | 13 | 14`, auto-detects the unique odd multiplier, and un-permutes the opcode bytes back to standard Luau `1`-multiplied opcodes!
   - **Android / `rustudio` VM advantage:** `rove-android`'s `Cargo.toml` already vendors `vendor/luau0-src` (`LBC_VERSION_MIN = 3`, `LBC_VERSION_MAX = 14`, `LBC_VERSION_TARGET = 13`), which is the exact same Luau 745 fork as `rove/vendor/luau`! Therefore, `rustudio-join` (or a small helper bridged into `rustudio-engine`) can execute the normalized, signature-verified bytecode directly in `mlua` (`luau_load` + call with `(second as f64, first as f64)`) — **no `rust-runtime-sys` C++ host required**.
6. **VM Environment Requirements for the `155` Program** (cross-checked with `rove/rust-client/src/program_check.rs` and `Boblox/FINDINGS.md:327-357`):
   - Arguments passed to the chunk: `(check.second as f64, check.first as f64)`.
   - `game.JobId`: must return `issued.game_id` (36-char UUID string).
   - `Random.new(seed)`: must support full 64-bit integer/double seeds without truncating `u2 + u1` above `2^32` (PCG state = `M * seed + C`, matching Roblox's `Random`). Verify `rustudio-engine`'s `Random` implementation uses 64-bit state and check `seed = (second as u64) + (first as u64)` precision in Luau (`f64` has 53 bits of exact integer mantissa, which covers 33-bit `u32 + u32` losslessly!).
   - Standard Luau sandbox globals/API checks expected by the program:
     - `bit32`, `xpcall`, `tostring`, `getmetatable`, `setmetatable`, `newproxy` (with `__namecall` support).
     - `RunService:IsStudio()` returning `false`.
     - `UserSettings()` callable and `tostring(UserSettings())` returning a stable non-empty Roblox-style object string (note `Boblox/FINDINGS.md:348-353` observed the native client's `UserSettings()` string check).
     - `os.exit` **absent** (`nil`) in the sandboxed Luau environment (`rustudio` already omits `os.exit`).
   - Return value conversion (`program_check.rs:250-260, 276-281`):
     - `let number = value.clamp(0., u32::MAX as f64).trunc() as u32;`
     - Reply bytes: `[155]` + `check.second.to_le_bytes()` + `number.to_le_bytes()` (9 bytes total), sent on `(4, 1)` (`REPLICATION_APPLICATION, REPLICATION_CHANNEL`).
7. **Live Integrity Observer (`SessionIntegrityObserver`)**:
   - Port `rove/rust-client/src/session_integrity.rs` alongside `ProgramAuth` so container checksum checks (`observe_container_comparison`) and physics/scheduler checks feed the real `integrity_status` word used in ping replies (never hardcoded `0` without checks having run).

### Why `ProgramAuth` Must Run Inside `rustudio` (Not as a Stub)

- Never hardcode or guess the `155` answer: `check.first` and `check.second` are random per-session `u32`s, and the bytecode inspects `game.JobId` and environment checks.
- Porting `rove/rust-client/src/program_check.rs` + `bytecode_format.rs` + `rove/inputs/bytecode-signature-v1-public.{bin,json}` + `mlua` execution (in a clean isolated `mlua::Lua` or `rustudio-engine` callback with `game.JobId` bound) gives genuine, cryptographically verified execution on Android ARM64.
- **Note on `mlua` bytecode loading:** Verify `mlua::Lua::load(normalized_bytes)` vs `luau_load` — `mlua` 0.12 accepts precompiled Luau bytecode when loaded via `chunk.into_function()` / `load(&[u8])` if `ChunkMode::Binary` (or `mlua::AsChunk` binary mode) is used. Verify `vendor/mlua` allows binary chunks in `luau` mode (default `mlua` compiles source unless `ChunkMode::Binary` is set or `Compiler` is bypassed; check `vendor/mlua/src/chunk.rs`!).

---

## 5. Gap 3 (Critical): Broken Ping Loop — Post-Globals Pings Ignored, Wrong Reply Words, Wrong Route

### Three Distinct Bugs in `crates/rustudio-join/src/{ping,session}.rs`

#### Bug 3A: Post-Globals Pings Are Completely Dropped
Look at `crates/rustudio-join/src/session.rs:495-501`:
```rust
(REPLICATION_APPLICATION, PING_CHANNEL, 131) => {
    if self.core.scope().is_none() {
        self.ping_requested(payload)
    } else {
        self.feed_inner_on(application, channel, payload)
    }
}
```
Once `GlobalsCommitted` sets `self.core.scope().is_some()`, every `(4, 3, 131)` packet goes into `self.core.ingest(payload)`, which decodes ping items into **`row.value.pings`** (`SessionRecord.pings`). Meanwhile, `Session::drain_batch()` (`session.rs:1160-1244`) **never reads `row.value.pings`**.
- **Contrast with `rove` (`rove/rust-client/src/live_session.rs:702-717, 2032-2035`):**
  Inside `observe(&entry.value)`, `rove` parses every entry in `record.pings` with `PingItem::from_record`, emits `Effect::Pings { items, received_clock_word }`, passes them to `ping_responder.receive_validated_items(&items, received_clock_word, &mut ping_telemetry)`, and flushes replies immediately.

#### Bug 3B: Wrong 6 `reply_words` and Missing `PingClock`
Look at `crates/rustudio-join/src/session.rs:933-943`:
```rust
fn ping_reply_words(&self) -> [u32; 6] {
    [
        word(self.ingested.packets),
        word(self.ingested.replication),
        word(self.document().dom.len()),
        word(self.journal_rows()),
        word(self.held),
        word(self.ingested.refused),
    ]
}
```
Look at `rove/rust-client/src/ping_responder.rs:589-599` & `live_ping.rs:28-104`:
```rust
reply_words: [
    received_clock_word,   // PingClock::word() when frame/datagram arrived
    enqueue_clock_word,    // PingClock::word() when added to PingResponder queue
    write_clock_word,      // PingClock::word() immediately before socket write
    stats.replication_word,// 0 on QUIC profile (negotiated 0x10_0000, no 0xe3)
    stats.tail_words[0],   // 0
    stats.tail_words[1],   // 0
]
```
The first three words are **session monotonic millisecond timestamps (`u32`)** (`received`, `enqueued`, `written`), NOT packet/DOM counts! Sending DOM counts as clock timestamps causes the server's ping/queue-delay estimator to see bogus multi-second/negative queue delays.

#### Bug 3C: Wrong Delivery Route for Unreliable Pings (`PING_WIRE_ID = 6`)
Look at `crates/rustudio-join/src/session.rs:910`:
```rust
self.outbound.push((crate::ping::PING_APPLICATION, crate::ping::PING_CHANNEL, reply));
```
`flush_outbound` writes every `outbound` entry via `ReliableSender::send`!
- **Contrast with `rove` (`rove/rust-client/src/live_ping.rs:106-170`):**
  Each `PingItem` carries an `unreliable` boolean (`route != 0` on the wire).
  - If `DeliveryRoute::Reliable` (`!item.unreliable`): send on `(4, 3)` via `ReliableSender`.
  - If `DeliveryRoute::Unreliable` (`item.unreliable`): first announce `control_open(4, 3, PING_WIRE_ID)` (`PING_WIRE_ID = 6`) on `(4, CONTROL_CHANNEL)`, then send via `send_unreliable(&connection, PING_WIRE_ID, seq, &payload)`.

### Exact Fix
Port `rove/rust-client/src/ping_responder.rs` and `rove/rust-client/src/live_ping.rs` (`PingClock`, `SessionPingTelemetry`, `PingResponder`, `PingWriter`) directly into `crates/rustudio-join/src/ping.rs`:
1. Stamp `received_clock_word = ping_clock.word()` at packet/datagram receive time.
2. Pre-globals `(4, 3, 131)` calls `ping_responder.receive_packet(payload, received_clock_word, &mut telemetry)`.
3. Post-globals `SessionRecord.pings` calls `ping_responder.receive_records(&record.pings, received_clock_word, &mut telemetry)`.
4. Flush `ping_writer.flush(&mut ping_responder, &mut ping_telemetry, &sender, &connection)` both on ping receipt and on the 20 ms tick (`live_session.rs:1999, 2034`).
5. Track `telemetry.observe_packet(application, payload)` for `0xe3` extended family detection and `negotiated = caps.negotiated()` (`0x10_0000`).

---

## 6. Gap 4 (Critical): `flush_outbound` Misclassifies Every `0x83` Packet as `PropertyAck`

### The Bug in `crates/rustudio-join/src/session.rs:1809-1833`

```rust
let is_ack = application == REPLICATION_APPLICATION
    && channel == REPLICATION_CHANNEL
    && reply.first() == Some(&0x83);
```
Because `0x83 == 131`:
- `PropertyAcknowledgments::prepare()` starts with `0x83` (`[0x83, ...]`) — **is** a property ack.
- `replication_out::streaming_ready()` starts with `131` (`[131, 3, ...]`) — **NOT** a property ack!
- `replication_out::marker_reply()` starts with `131` (`[131, 4, ...]`) — **NOT** a property ack!
- `replication_out::replication_focus()` starts with `131` (`[131, 26, ...]`) — **NOT** a property ack!

So sending `marker_reply`, `streaming_ready`, or `replication_focus` sets `ack_written = Some(true)`, increments `session.property_acks_sent` falsely, and calls `session.property_ack_written(true)` even when a real `PropertyAck` was *not* sent (or after a real `PropertyAck` earlier in the same `queued` batch failed, overwriting `ack_written = Some(false)` with `Some(true)`!).

### Exact Fix

1. Replace `Vec<(u8, u32, Vec<u8>)>` with a typed outbound enum (mirroring `rove`'s `Effect`):
   ```rust
   pub enum OutboundPacket {
       Reply { application: u8, channel: u32, payload: Vec<u8>, kind: OutboundKind },
       PropertyAck(PreparedPropertyAcks),
   }
   ```
2. Only commit `self.property_acks.commit(prepared.id(), written)` for `OutboundPacket::PropertyAck(prepared)` — **immediately on its own send result**, never inferred from byte `0x83`!
3. Also note `rove/rust-client/src/live_session.rs:462-465` vs `session.rs:1284`:
   In `session.rs`, `acknowledge_properties` calls `self.property_acks.prepare(&view)` right after `observe()`, which returns `None` while `ack_in_flight` is `Some`. **Remember to call `prepare_next_property_ack()` again right after `property_ack_written(true)` commits**, otherwise queued `CFrame` versions sit pending until the *next* `property_changes` packet arrives!

---

## 7. Gap 5: Marker Queue Readiness, Streaming Resume, Focus Loop, and `character_present()`

### 5A. `MarkerQueue` Gating (`initial_world_tag_received` / Tag `13`)
- **Current `session.rs:528-544`:** Calls `marker_reply(payload)` and pushes straight to `outbound`, even before `client_scope` or `initial_world_tag_received` (`record.tags.contains(&13)`).
- **Upstream `rove` (`live_session.rs:786-798` + `replication_out.rs:142-175`):**
  - Queues every `132` packet in `MarkerQueue::add(payload)`.
  - Sets `self.initial_tag = true` when `record.tags.contains(&13)` (`live_session.rs:687-695`).
  - Calls `markers.drain(MarkerReadiness { client_scope_received: self.core.scope().is_some(), initial_world_tag_received: self.initial_tag })` after each record!
  - Port this exact `MarkerQueue` + `initial_tag` tracking into `session.rs`.

### 5B. `character_present()` Matches Any Player Instead of Local `UserId`
- **Current `session.rs:843-855`:**
  ```rust
  fn character_present(&self) -> bool {
      self.player_ids.iter().any(|id| { ... })
  }
  ```
  On Place `1818` (9 players), `character_present()` returns `true` as soon as *any other player* has a replicated `Character`!
- **Upstream `rove` (`live_session.rs:607-656, 799-813`):**
  - `local_player()` resolves the single `Player` node whose `UserId` matches `self.user_id` (`u64`, with `i64`/`f64` safety — never truncated to `i32`!).
  - `evaluate_spawn_evidence(schema, &view, player_id, &self.region_records)` checks **only that `local_player`** for `Character` + `Humanoid` + `RootPart` + `GameplayPaused` property ID + spawn region coverage.
- **Exact Fix:** Replace `character_present()` with `local_player()` + `evaluate_spawn_evidence(...)` from `rove/client-core/src/` (or `live_session.rs:94-240`). Also remove the hardcoded `9` in `streaming_ready(..., 9, ...)` or read `property.type_id` from schema (asserting `9`, matching `streaming_ready`).
- Also support `replication_out::request_character(player_guid)` (`[131, 49, ...]`) cleanly as an explicit fallback/trigger if a streaming place acknowledges `GameplayPaused = false` (`streaming_ready`) and still does not deliver `LocalPlayer.Character` after a bounded wait, or verify whether Place `1818`/`189707` needs it once `155` + `PingResponder` are fixed.

### 5C. `LiveWorld::set_focus` Is Never Called by the Editor/Player!
- `LiveWorld::set_focus` (`crates/rustudio-join/src/live_world.rs:302`) exists, and `session.rs:1687` reads `live_world.focus()`.
- **However, `grep -rn "set_focus"` across the entire repository shows zero callers!**
- **Exact Fix:** Every frame in `Editor::live_join_frame()` (or `play.rs` tick when `mode == RunMode::Client`):
  - If `play.character` is bound, read the local character root (`HumanoidRootPart` / `Torso`) world position (or fallback `self.cam.focus` / `self.cam.pos`) and call `live.set_focus([pos.x, pos.y, pos.z])`.
  - Keep `FOCUS_RADIUS_STUDS` / `FOCUS_REFRESH` in `session.rs` so the streamer streams parts around the player's actual character/camera rather than remaining pinned to the initial spawn region coordinate forever.

### 5D. Non-Streaming Places (`received_world_ready.rs`)
- Port `rove/rust-client/src/received_world_ready.rs`: when `Workspace.StreamingEnabled == false` and `initial_world_tag_received == true`, simulation is ready without waiting for `region_records`.

---

## 8. Gap 6: Client-to-Server Gameplay Uplinks (`PhysicsSender`, `IsrSender`/`IsrClock`, `RpcSession`, `TouchContact`)

Once the world renders and your character spawns, **continuous gameplay** requires sending client actions back to the server. `rove-android` currently sends *none* of these four uplinks:

### 6A. Character Root Physics Uplink (`PhysicsSender` on `(4, 6)` / `WIRE_ID = 8`)
- **Reference:** `rove/rust-client/src/physics_sender.rs` + `physics_codec.rs` + `physics_rotation.rs` + `live_session.rs:2000-2021`.
- **How it works:**
  1. When `RunMode::Client` steps the local owned character (`live_character_owned() == true`), produce a lightweight `RootPhysicsFrame`:
     - `root_part_id: u64` (the `(scope << 32) | index` parsed from `Live`'s `scope:index` ID of `Humanoid.RootPart` / `HumanoidRootPart`)
     - `position: [f64; 3]`, `rotation: [f64; 9]` (row-major 3×3 matching `Xform`), `velocity: [f64; 3]`, `grounded: bool`
  2. Push the latest `(frame, Instant::now())` into a shared lock-free / `Arc<Mutex<Option<(RootPhysicsFrame, Instant)>>>` on `LiveWorld` (latest-wins, discarding frames older than `250 ms` just like `live_session.rs:2002`).
  3. On the session's 20 ms tick:
     - First time: send `control_open(4, 6, 8)` on `(4, CONTROL_CHANNEL)`.
     - Encode via `physics_sender::encode_root_frame_with_rotation(root_part_id, clock.physics_milliseconds()?, physics_seq, pos, rot, vel, grounded)?`.
     - Send via `send_unreliable(&connection, 8, physics_seq, &payload)`.
- **Why this matters:** Without `(4, 6)` datagrams, walking on the phone moves your local mesh while the server (and every other player) sees your character frozen at spawn — or worse, the server continuously snaps your `CFrame` back to spawn.

### 6B. Owned Loose-Part ISR Uplink & Clock (`IsrSender` + `IsrClock` on `(4, 5)` / `WIRE_ID = 10`)
- **Reference:** `rove/rust-client/src/isr_sender.rs`, `rove/rust-client/src/isr_clock.rs`, `rove/rust-client/src/live_session.rs` (`ready_owned_parts`, `isr_clock`).
- **How it works:**
  - `IsrClock` observes incoming server `161` timestamps (`SessionCore::instance_state_report()`) to learn the remote/local step offset (never guessed).
  - For client-owned unanchored non-character assemblies (`Owner::owns` where `NetworkOwnerV3 == self_peer` and `!Anchored`), `IsrSender::record(schema, &view, &part)` encodes `ReplicationPV` (schema property type `57`) and `IsrSender::packet_with_offset` packs up to 8 records (≤ 1100 bytes) into a `161` datagram on `WIRE_ID = 10` (`control_open(4, 5, 10)`).

### 6C. `RpcSession` (`RemoteEvent`, `UnreliableRemoteEvent`, `RemoteFunction`, `TouchContact`)
- **Reference:** `rove/client-core/src/rpc.rs`, `rove/rust-client/src/live_session.rs:433-435, 499-542`, `rove/client-core/src/replication_out.rs:58-135`.
- **How it works:**
  1. On `GlobalsCommitted`, construct `AnnouncedPlayers::from_globals(scope, &view)`.
  2. Once `local_player` is resolved, initialize `RpcSession::new(scope, user_id, parse_instance_guid(&player)?, 0x100000)` and retain `AnnouncedPlayers`.
  3. On each committed `SessionRecord`:
     - Call `rpc.observe_record(scope, schema, &view, record, &active_requests)`.
     - Forward inbound `ServerEvent` / `ClientCall` / `RemoteResult` to `LiveWorld` so `rustudio-player` dispatches `OnClientEvent` / `OnClientInvoke` / resumes yielded `InvokeServer()` coroutines inside the `Side::Client` Luau VM!
  4. From `rustudio-engine` / `rustudio-player` (`RunMode::Client`):
     - `RemoteEvent:FireServer(...)` → `LiveWorld` outbound RPC queue → `rpc.fire_server(...)` → `(4, 1)` (or unreliable datagram channel for `UnreliableRemoteEvent`).
     - `RemoteFunction:InvokeServer(...)` → `LiveWorld` outbound RPC queue → `rpc.invoke_server(...)` → yield coroutine until correlated `RemoteResult` returns.
     - `ClientInvoke` replies → `rpc.reply_client(...)`.
     - Local character touch transitions (`BasePart.Touched` / `TouchEnded` involving server/other parts) → `replication_out::contact_step_batch` (`134`) sent reliably on `(4, 7)`.

---

## 9. Engine, Ownership, and Rendering Bridge Details

### 9A. `Document::owner()` and `Terrain.NetworkOwnerV3` (`live_physics.rs` Rule)
- Look at `crates/rustudio-core/src/doc.rs:238-257`:
  ```rust
  pub fn owns(&self, doc: &Document, r: Ref, root: Ref) -> bool {
      if !doc.replicated_peer.contains_key(&r) && !doc.replicated_peer.contains_key(&root) {
          return true;
      }
      let Some(me) = self.self_peer else { return false };
      peer_of(doc.prop(root, "NetworkOwnerV3")) == Some(me)
  }
  ```
- Compare with `rove/rust-client/src/live_physics.rs:101-114`:
  - `rove` reads `Terrain.NetworkOwnerV3` as `server_owner_id`:
    ```rust
    match self.server_owner_id {
        Some(server) => owner != server,
        None => Some(owner) == self.client_scope,
    }
    ```
    and checks `!Anchored` and `!ServerAuthorityMode`.
  - Also check `play.rs:2292-2320` (`live_character_owned`):
    Currently, `live_character_owned()` returns `true` if `NetworkOwnerV3` is `None`, and `false` if `NetworkOwnerV3` is `Some(_)` with `peer_of == None`. Watch out for `NetworkOwnerV3 == 1` (`SERVER_NETWORK_OWNER` or `Terrain.NetworkOwnerV3`) vs `self_peer`: make sure `live_character_owned()` matches `Owner::owns` + `ServerAuthorityMode` check cleanly, and log the exact `NetworkOwnerV3` integer value and `self_peer` in `live_datamodel_report()` when the character binds.
- **Warning about `claim_streamed_parts()` (`play.rs:2198-2246`):**
  Every frame, `claim_streamed_parts()` scans all `BasePart`s under `Workspace`, compares against `s.live_claimed`, and calls `s.physics.claim_owned(&mut self.doc, owner, &fresh)`.
  - **Bug to watch out for:** If `NetworkOwnerV3` changes on an *already-claimed* part (for example, the server hands an unanchored part to `self_peer` when the player walks near it, or takes ownership back), `fresh` only contains newly-created `Ref`s not in `claimed`! `claim_owned` will **not** see ownership transitions on existing `Ref`s unless `Live::set` / `claim_streamed_parts` also checks dirty `NetworkOwnerV3` changes!
  - **Fix:** Track dirty `NetworkOwnerV3` refs (or re-evaluate dynamic/unanchored parts whose `NetworkOwnerV3` property changed in `doc` marks) so `s.physics.claim_owned` updates dynamic vs kinematic status when ownership transfers mid-session.

### 9B. `UserId` 64-bit Safety (`Boblox/HANDOFF.md` + `play.rs` + `session.rs`)
- Modern Roblox `UserId`s exceed `2^31` (e.g. `4656429295`).
- Check every place `UserId` is stored or compared:
  - `session.rs:801`: `view.get_property(id, "UserId").and_then(|v| v.as_f64()).is_some_and(|v| v as u64 == uid)` — verify how `SessionCore` serializes `UserId` into `WorldView` (`i64`/`u64`/`f64`/`$number`!). If `WorldView` stores `Value::Number` from `i64` or `{"$number": ...}`, `v.as_u64().or_else(|| v.as_i64().map(|n| n as u64)).or_else(|| v.as_f64().map(|n| n as u64))` is much safer than `.as_f64()` alone!
  - `play.rs:2395-2400`: handles `Variant::Int64` and `Variant::Int32` (`*v as u64`). Add `Variant::Float64(v)` just in case `snapshot.rs` decodes `$number` as `Float64`.
  - Check `snapshot.rs:200-275`: verify `Player.UserId` (type `Int64`) converts to `Variant::Int64` and is never truncated to `Int32`.

### 9C. `Live::create` & `Scene::sync` Visibility Checks
1. **Check `Workspace.CurrentCamera` (`play.rs:2085-2138` & `2449-2481`):**
   - `setup_live_client_player` sets `CameraType = Custom` before the character exists, and `bind_live_character` sets `CameraSubject = Humanoid` and moves `self.cam` to the character's `CFrame`.
   - **Important:** Before the character spawns, if `self.cam.pos` is still at the editor's default origin/far coordinates while the replicated map is at e.g. `(0, 50, 0)` or `(500, 100, -300)`, frame the camera onto the first replicated `SpawnLocation` or `Workspace` `BasePart` bounding box center so the user sees the streaming map immediately under the top banner instead of an empty skybox!
2. **Check `PlayerGui` / `StarterGui` (`play.rs:2422-2447`):**
   - `setup_live_client_player` clones `StarterPlayerScripts` into `Player.PlayerScripts`. Check whether the server replicates `Player.PlayerGui` directly or whether `StarterGui` children should be cloned into `PlayerGui` once `LocalPlayer` binds if the server hasn't replicated `PlayerGui` yet (avoiding duplicates if `PlayerGui` already arrived via replication).
3. **Check `workspace_summary(doc)` in `src/join_window.rs:83-114`:**
   - Once `RuntimeDeltaNormalizer` is active, `workspace_summary` should immediately flip from `0 with a CFrame, N without Size` to real framed & sized parts. Add `Terrain` voxel/presence info (`has_terrain`) and `MeshPart`/`SpecialMesh` counts to `workspace_summary` so empty terrain-only maps are distinguishable from broken `Part`s.
4. **Check `Live::apply` Ref-Resolution When `Live` Starts Mid-Stream (`src/join_window.rs:1022-1048`):**
   - Remember `join_window.rs` runs `self.start_play(RunMode::Client)` one frame after `self.join_win.installed` becomes `true`. Verify `start_play(RunMode::Client)` **never** clones or replaces `self.doc` (only `RunMode::Play` clones `other_doc`!), so `join_win.live_local.matches(&self.game.doc)` remains `true` for 100% of the session.

---

## 10. Boblox Protocol & Engine Cross-Check (Transfers vs Non-Transfers)

From `kingdudely/Boblox` (`FINDINGS.md`, `HANDOFF.md`, `docs/OWNERSHIP.md`, `docs/NDS_CONNECTION.md`, `docs/PARITY.md`), here is the strict triage of what transfers to `rove-android` vs what does not:

### Directly Transfers to `rove-android`
1. **Place `1818` vs Place `189707` (`Boblox/HANDOFF.md`):**
   - Place `1818` (Classic Crossroads / bare UGC test place) has minimal/no `PlayerModule`/complex scripts, whereas **Place `189707` (Natural Disaster Survival)** exercises full terrain/map streaming, `CharacterAutoLoads`, `RemoteEvent`s, animations, and multi-player movement. Test **both** `1818` (fast smoke test for `155` + `CFrame`/`Size` + spawn) and `189707` (full M3 gameplay target).
2. **`NetworkOwnerJob::updatePlayerLocations` (`Boblox/docs/OWNERSHIP.md`):**
   - The server skips any client lacking a valid `ReplicationFocus` + simulation region before checking `Player.Character` root for network ownership.
   - `NetworkOwnerV3` is **server-granted** (`SetNetworkOwner` is server-only); the client must never fabricate `ReplicationFocus` or `SimulationRadius` *property* writes on instances (only send the `ReplicationFocus` protocol item `[131, 26, ...]`).
3. **Server vs Client Physics Split (`Boblox/docs/NDS_CONNECTION.md`):**
   - Map parts and other players' assemblies stay server-simulated (`dynamic=0` / kinematic targets updated from replication); only `LocalPlayer`'s owned character root and server-granted assemblies (`NetworkOwnerV3 == self_peer && !Anchored`) run local dynamic physics.
4. **64-bit `UserId` (`Boblox/HANDOFF.md`):**
   - Never narrow `UserId` to signed 32-bit anywhere in join matching or Luau globals.
5. **Single Active Session Per Account (`Boblox/FINDINGS.md:283-286`):**
   - Never join the same place simultaneously with two clients on the same `.ROBLOSECURITY` account; the server resets the stream (`0x106`).
6. **`0x9B` (`155`) Challenge Semantics (`Boblox/FINDINGS.md:315-385`):**
   - Confirms `rove/rust-client/src/program_check.rs`: `(check.second, check.first)` argument order, `game.JobId`, 64-bit `Random.new(u2 + u1)`, `RunService:IsStudio() == false`, `UserSettings()`, `newproxy` `__namecall`, `bit32`, `os.exit == nil`, and `[155, u2, answer]` reply.

### Does **Not** Transfer (RakNet-Only in `Boblox`)
- Legacy RakNet `0x83` framing, `0xA1` bitmask `classHasWriterSlots()` workarounds, `0xA6` RakNet pings, and `libngtcp2` browser plans do **not** apply — `rove-client-core` already handles WebTransport/QUIC framing, `151` schema, `129` globals, `131` replication, `161` ISR, and `134` contacts natively.

---

## 11. Ordered Implementation Plan for a Coding Agent

Execute these steps in strict order so every layer can be verified independently on Android without masking earlier failures:

### Step 1 — Fix `RuntimeDeltaNormalizer` & Single-Record Receipt Processing (`rustudio-join` + `rustudio-live`)
1. In `crates/rustudio-join/src/session.rs`:
   - Add `normalizer: Option<rove_client_core::runtime_delta::RuntimeDeltaNormalizer>` and `initial_tag: bool` to `Session`.
   - Mirror `rove/rust-client/src/live_session.rs:378-493`: capture `let pre_view = self.core.world_view();` before `core.ingest` / `core.ingest_instance_state`; read the committed `SessionRecord` (`read_after(self.reader, self.acknowledged, 1)`); track `self.initial_tag |= record.tags.contains(&13);`; initialize `RuntimeDeltaNormalizer::new(schema)` once `schema` is available; normalize the record via `RuntimeDeltaNormalizer` (verify `GlobalsCommitted` handling when `pre_view` is `None` vs empty `WorldStore::new().view()`); apply & publish `normalized.worker_delta["actions"]`; acknowledge `self.core.acknowledge(self.reader, sequence)`.
2. Verify `crates/rustudio-live/src/cframe.rs` & `snapshot.rs` convert every `normalized` `CFrame` (identity + non-identity `rotation_id` + `$cframe` matrix/quaternion) and `Size` cleanly.
3. **Unit test:** Replay a captured Place `1818` / `189707` session containing `ReplicationPV` and lowercase `size` and assert `Workspace` `BasePart`s have non-default `CFrame` and `Size` in `Document`.

### Step 2 — Fix `flush_outbound`, `MarkerQueue`, `local_player` / `spawn_evidence`, and `LiveWorld::set_focus`
1. Replace `Session::outbound` with typed `OutboundPacket` so `PropertyAck` commits *only* for `OutboundPacket::PropertyAck` (never `reply[0] == 0x83`), and re-call `prepare()` after `commit(id, true)`!
2. Port `rove_client_core::replication_out::MarkerQueue` and gate replies on `MarkerReadiness { client_scope_received: self.core.scope().is_some(), initial_world_tag_received: self.initial_tag }`.
3. Replace `character_present()` with `local_player()` matching `account_user_id` (`u64`) + `spawn_evidence` + `received_world_ready::can_simulate`. Add `request_character` (`[131, 49, ...]`) option if `GameplayPaused` clears without character delivery.
4. Call `live.set_focus(...)` every frame from `src/join_window.rs` (`live_join_frame`) using the bound character root position (or camera target while loading).
5. Frame the initial live camera onto the first `SpawnLocation` / `Workspace` part center before the character arrives.

### Step 3 — Port `PingResponder` + `PingWriter` + `SessionPingTelemetry` + Post-Globals Pings
1. Port `rove/rust-client/src/ping_responder.rs` and `rove/rust-client/src/live_ping.rs` into `crates/rustudio-join/src/ping.rs`.
2. Stamp `received_clock_word = ping_clock.word()` when reliable frames (`(4, 3)`) or datagrams arrive.
3. Pre-globals `(4, 3, 131)` → `ping_responder.receive_packet`; post-globals `record.pings` → `ping_responder.receive_records`.
4. Flush `PingWriter` (supporting both reliable `(4, 3)` and unreliable `PING_WIRE_ID = 6` via `control_open(4, 3, 6)`) on ping receipt and on the 20 ms tick.
5. Expose `ping_requests` / `ping_replies` / `ping_pending` in `live_world::Counters` and the join overlay.

### Step 4 — Port `155` (`0x9B`) `PublicTrust` + `ProgramAuth` + `bytecode_format` via `mlua`
1. Port `rove/rust-client/src/program_check.rs` (`ProgramCheck`, `PublicTrust`, `ProgramAuth`, `unpack_container_observed`), `rove/rust-client/src/bytecode_format.rs`, and `rove/rust-client/src/session_integrity.rs` into `crates/rustudio-join/`.
2. Embed `rove/inputs/bytecode-signature-v1-public.bin` (`include_bytes!`) and `.json` (`include_str!`) so Android needs no external asset files.
3. Implement `run_verified_program_luau(&verified, &job_id)` using `rustudio`'s vendored `mlua` (Luau 745, `LBC_VERSION_MIN..=MAX = 3..=14`):
   - Verify signature via `PublicTrust::verify` *before* touching the VM.
   - Normalize opcode multiplier via `bytecode_format::normalize(body, None)`.
   - Load binary bytecode into an isolated `mlua::Lua` chunk with `game.JobId`, `Random` (64-bit seed), `bit32`, `newproxy` (`__namecall`), `RunService:IsStudio() -> false`, `UserSettings()`, `os.exit == nil`, and call with `(check.second as f64, check.first as f64)`.
   - Return `[155, check.second.to_le_bytes(), number.to_le_bytes()]` on `(4, 1)` and record `program_answered += 1`.
4. **Unit test:** Run `program_check.rs` + `bytecode_format.rs` tests plus an `mlua` binary-load execution test with `mul(227)` bytecode on host & Android CI.

### Step 5 — Implement Client-to-Server Gameplay Uplinks (`PhysicsSender`, `IsrSender`, `RpcSession`, `TouchContact`)
1. **Character root physics (`(4, 6)` / `WIRE_ID = 8`):**
   - Port `rove/rust-client/src/{physics_sender,physics_codec,physics_rotation}.rs` into `crates/rustudio-join/`.
   - Add `LiveWorld::push_character_pose(RootPhysicsFrame)` from `rustudio-player`'s `RunMode::Client` tick when `live_character_owned()` is true.
   - Send on the session's 20 ms tick (staleness cap 250 ms, `control_open(4, 6, 8)` once).
2. **Owned-part ISR (`(4, 5)` / `WIRE_ID = 10`):**
   - Port `rove/rust-client/src/{isr_sender,isr_clock}.rs` and update `claim_streamed_parts()` in `play.rs` to also detect `NetworkOwnerV3` changes on existing parts.
3. **RPC & Contacts (`RpcSession` + `(4, 7)` `134`):**
   - Wire `rove_client_core::rpc::{RpcSession, AnnouncedPlayers}` into `Session`, bridging `RemoteEvent`/`UnreliableRemoteEvent`/`RemoteFunction` and `contact_step_batch` across `LiveWorld` ↔ `rustudio-player` `Side::Client` runtime.

---

## 12. Verification & Telemetry Acceptance Criteria (Android In-App Report)

After implementing Steps 1–4 (before Step 5) and again after Step 5, test Place `1818` and Place `189707` on Android and verify all of the following in the in-app **Join Report / Output** (`join_window.rs` & `live_datamodel_report()`):

1. **Auth & Handshake:**
   - `program_challenges > 0` and **`program_answered == program_challenges`** (no refused `155` challenges).
   - `markers` drained cleanly after `self_peer != 0` and `initial_tag == true`.
   - `ping_replies == ping_requests` (continuous growth post-globals, `ping_pending` near 0).
   - `streaming_ready` = `acked` only after `local_player` (`UserId == account.user_id()`) and region/character evidence exist.
   - `property_acks_sent` matches actual `PropertyAcknowledgments` packets sent (no inflation from `0x83` control frames).
2. **Normalized World & Map Visibility:**
   - `join world: Workspace has N BasePart(s): >0 with a CFrame, 0 without Size` (no more `Part.CFrame ×0` / `Part.Size ×0`).
   - Camera frames the map immediately under the top banner, not an empty skybox.
3. **Character & Continuous Gameplay (Step 5):**
   - `Live: character bound` succeeds for `LocalPlayer`, `CameraSubject = Humanoid`, touch thumbstick + jump work.
   - `Live: network ownership now: >0 part(s) simulated here` (our character's parts).
   - `physics_frames_sent` advances smoothly at ~50 Hz while character moves; server accepts motion without rubber-banding back to spawn.
   - On Place `189707` (NDS), countdown/lobby UI (`PlayerGui` + `RemoteEvent`s) updates live and gameplay continues without disconnects or freezes.
