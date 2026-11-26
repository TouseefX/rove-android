# Humanoid state observations

`GetState` and state callbacks use the common HumanoidStateType enum binding.
The type now participates in enum enumeration, name/value lookup, and normal
value serialization, including Actor messages. Received numeric and tagged enum
values are checked before conversion; large integers cannot wrap into another
state.

The Rust runtime publishes ordered `StateChanged(old, new)` events for received
HumanoidStateType updates and accepted local physics feedback. A received state
clears the older local feedback override. Equal states and stale retries are
silent. Unknown initial state stays unknown; receiving a first state establishes
it without inventing an old state or transition.

Transitions also notify entry and exit for FallingDown, Ragdoll, GettingUp,
Jumping, FreeFalling, Strafing, and PlatformStanding. Running is exposed as a
speed signal driven by actual running-speed feedback. No swimming or climbing
speed is inferred from a state name. SetStateEnabled publishes the local
StateEnabledChanged event when its setting changes. The event arguments follow
the [Humanoid reference](https://create.roblox.com/docs/reference/engine/classes/Humanoid).

Physics feedback validates the Humanoid, state, speed, root, and velocity before
committing those observations or publishing events. Existing Actor VMs receive
these validated observations directly, without an extra persistent feedback
queue or a full world copy. Actor synchronization keeps root-owned state and
running-speed observations authoritative, including removal of an old feedback
override by received state. Deferred callbacks see committed values; each
StateChanged payload still retains its own old/new enum items.

The session's engine-pose operation remains separate from feedback observation
validation. This change does not claim transaction rollback across a combined
pose-plus-feedback command, native immediate signal scheduling, complete
Humanoid state simulation, or fresh server acceptance. Local state-enable
commands remain part of the existing physics-intent path.

Verification uses the official Luau VM on macOS, authored received/feedback
fixtures, Actor messages, and the offline local-physics orbit test. Windows
execution and RAM measurements are separate verification work.
