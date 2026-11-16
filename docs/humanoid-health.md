# Humanoid health

The Rust host exposes `HealthChanged`, `Died`, and `TakeDamage` on Humanoid
instances. Local health writes, received updates, and Actor synchronization
preserve each health-event payload. Property-specific Health listeners still
receive no arguments. Equal values and stale retries do not repeat events, and
rejected receipts publish nothing.

`TakeDamage` supports positive damage and negative healing. It respects force
fields beside the Humanoid, under its root part, or under an ancestor other than
Workspace. Visibility does not affect protection. Direct Health assignment
bypasses force fields. Finite extreme inputs clamp without arithmetic overflow;
invalid arguments fail before mutation. Healing requires a known MaxHealth, and
missing received Health remains unavailable.

Local MaxHealth changes notify health listeners with the resulting health.
When a lower maximum clamps Health, the existing two mutation rows retain one
health notification: the maximum row carries an internal
`_humanoid_health_silent` marker, followed by the actual Health transition.
Actor replay still uses the ordinary guarded property setters. Local death
transitions also record their event decision with `_humanoid_died`, preserving
suppression when engine state settings change before another VM catches up.
Received network rows do not use either local journal marker.

Received health is authoritative. The notification path does not clamp received
values, synthesize missing values, or generate outbound mutations. Increasing
Health above an already reached known maximum suppresses HealthChanged while
ordinary property notifications remain intact. A first received zero does not
prove a transition from alive to dead.

Died requires known positive health falling to zero or below, Workspace ancestry, and an
enabled Dead state. Ordered received reparenting is considered at the health
transition. These rules follow the
[Humanoid reference](https://create.roblox.com/docs/reference/engine/classes/Humanoid).

The receipt planner retains only touched Humanoid values and changed hierarchy
headers. It has no persistent duplicate health table, and batches without health
updates return before collecting property fields or reading instance values.

This is local runtime behavior and offline verification through the official
Luau VM. It does not establish server-authoritative damage, automatic joint
breaking, complete death-state simulation, regeneration, fresh live-server
acceptance, Windows execution, or a measured RAM reduction.
