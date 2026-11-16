# Attribute notifications

Instances expose `AttributeChanged` alongside `GetAttributeChangedSignal(name)`.
The general signal supplies the changed name; the specific signal supplies no
arguments. These signatures follow the [Roblox attribute reference](https://create.roblox.com/docs/scripting/attributes).

Received single-key updates and full attribute snapshots now plan notifications
in receipt order. A value changing from 0 to 8 and back to 0 generates both
changes. Equivalent consecutive values, stale retries, and rejected transactions
generate no extra notifications. A known nil remains distinct from unavailable
attribute data. Receiving one known key does not claim the whole map is known.

The root VM and Actor VMs share the same notification planner. Actor frame sync
carries received attribute changes before the root's local journal, preserving
received changes and local round trips even when the final value matches the
Actor's previous snapshot. Local `SetAttribute` continues to use its existing
validation and signals; the planner supplies notifications to receiving VMs.

Only touched attribute maps are copied for comparison, with no instance property
snapshot for a single-key attribute update. Unrelated creates are not copied.
Ordinary movement receipts skip this planner. A shared-world fixture checks that
one attribute update materializes only its target record among 1,000 parts.
This is an allocation assertion, not a peak RAM benchmark.

Command-loop tests use the official Luau VM and cover root and Actor callbacks,
local and received round trips, full snapshot removal, waiting coroutines,
unknown state, known nil, retries, and rollback. No fresh server session or
Windows execution is established by these offline tests. Callbacks are deferred
and read the committed state; attribute events do not carry historical values.
Scalar property-change batching is covered separately in
[property notifications](property-notifications.md).
