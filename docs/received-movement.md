# received movement

The Rust live client now routes received app 4 / channel 5 / packet 161 (ISR)
through the world owner and into the Luau worker. These packets previously
fell through the unhandled-packet branch, leaving scripts with the initial
replicated part positions.

Each accepted record updates its own scoped instance ID. Movement and ordinary
replication share the same world/journal sequence. The receiver rejects malformed
packets before changing the world, skips unknown or mismatched instances, and
keeps a per-instance timestamp fence with u32 rollover and instance recreation
handling. It does not manufacture instances or movement acknowledgements.

The decoder supports the traced legacy PV format at all four LODs, including
position, rotation and linear/angular velocity. That format is enabled only by
the received `DebugAdaptivePVCompressor=false` configuration. Unsupported formats
are counted in `instance_state_replication`; they are not guessed.

The runtime projects `ReplicationPV` into `CFrame`, `Position` and velocity reads.
Worker updates clear older local transform overrides in the root and Actor VMs.
Fields projected from a typed PV observation carry a bounded per-action physics
marker, so these updates do not emit BasePart property-change signals. Explicit
CFrame/velocity fields and unrelated property edits retain their notifications.
The comparison state still advances through silent updates, preserving ordered
comparisons with later explicit edits in the same batch. This corrects the
initial implementation and its notification test expectations, following the
[Object signal limitations](https://create.roblox.com/docs/reference/engine/classes/Object#GetPropertyChangedSignal).
The world store stamps the received sequence on the bounded set of motion
properties. This lets shared projections choose newer PV data over an older
explicit pose, and newer explicit edits over older PV data. An explicit frame or
velocity wins a same-receipt tie. These revisions follow the existing world
transaction, including rollback and pinned old views. Unrelated properties do
not acquire new revision entries.

CFrame comparisons use transform components, ignoring PV packet metadata. An
explicit edit to the already-observed pose therefore stays silent.
Shared-world reads derive these values without retaining a second set of
canonical properties. Group timing bytes are preserved by the decoder; no
unverified clock conversion or interpolation is applied.

Regression coverage includes captured PV fields, malformed packets, out-of-order
timestamps, journal backpressure, instance recreation, the live world handler,
and a real Luau sampler that reads separate player roots across `task.wait`.
The sampler runs through both shared-world and ordinary delta delivery. Actor
checks cover old local pose overrides, current reads, mixed physics/property
rows, ordinary local writes, and successive silent and explicit observations.

For receive-only validation against a locally saved capture:

```sh
cargo run --locked --manifest-path rust-client/Cargo.toml \
  --example replay_movement_capture -- CAPTURE.json REPORT.json
```

This streams only received world/configuration/movement records and writes
aggregate movement counts. It makes no connection, sends no captured traffic,
and cannot establish fresh-server acceptance. Live reports expose the same ISR
decode/apply/reject counters.

The initial receive-path verification passed 212 distinct tests. Its original
physics-signal assertions are superseded by the behavior described above. A saved Jailbreak capture
decoded all 1,544 ISR packets without a decode failure: 6,561 records applied,
12 records skipped because their instances were unavailable, and 51 instances
changed position through ISR. Seven player root parts were still present at the
end of the capture; six had recorded movement. These are offline results, not
a new live-session or Windows execution result.
