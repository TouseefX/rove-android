# Assembly grounding

`BasePart:IsGrounded()` now queries the current assembly anchoring state in the
Rust Luau host. An anchored part returns true; an unanchored part returns true
when its active rigid assembly contains an anchored member. Contact with a floor
does not count as grounding. This follows the
[BasePart API definition](https://create.roblox.com/docs/reference/engine/classes/BasePart#IsGrounded).

The method reuses the existing rigid-joint graph, including its enabled-joint and
Workspace/WorldModel rules. Local changes and received updates are visible to
subsequent queries, including queries inside Actor VMs. Missing anchoring or
unresolved required joint data produces an unavailable-state error instead of a
fabricated false result. This adds no new joint kinds or physics simulation.

Regression coverage includes anchored chains, disabled and destroyed joints,
separate world roots, floor contact, incomplete snapshots, invalid receivers,
Actor updates, and shared received storage. A storage test checks that querying
an assembly leaves 1,000 unrelated shared parts unmaterialized and emits no
script-authored mutations.

These are offline macOS tests using the official Luau VM. They do not establish
fresh live-server behavior, Windows execution, exact native assembly splitting,
or a measured memory reduction.
