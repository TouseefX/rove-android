# Script velocity and Physics state

A script write to the current local character root's `AssemblyLinearVelocity`
now emits an ordered movement intent as well as its ordinary local property edit.
The live predictor consumes the intent after checking the current received
player, character, root, network owner, anchoring, and streaming evidence. Edits
to other parts remain local property edits; this path does not simulate arbitrary
owned bodies or welded assemblies. Vector components must be finite and within
one million studs per second. Queue capacity is checked before the property edit.

`Humanoid:ChangeState(Enum.HumanoidStateType.Physics)` now disables the predictor's
horizontal walk controller. Gravity and static collision stepping continue,
and movement/jump commands do not automatically leave this state. Running or
Freefall requests restore the existing automatic controller. The predictor also
consumes enable/disable requests for these three states. Other state requests
are counted as rejected rather than being presented as implemented simulation.
This follows the [documented Physics state](https://create.roblox.com/docs/reference/engine/enums/HumanoidStateType#Physics),
which supplies no Humanoid force and requires an explicit state change to exit.

An explicit velocity survives empty scheduler ticks until a physics step uses
it. In ordinary controller mode the existing walk controller can replace its
horizontal components on subsequent steps. Physics mode preserves them until
collision, another velocity write, or a received correction changes them. This
is still the existing bounded static-AABB solver, not Roblox's full Humanoid
controller, contact friction model, or articulated character solver.

Accepted simulation feedback carries the Physics state, actual velocity, and
position back to Luau. It retains the existing silent BasePart physics-update
rules. A fresh received state supersedes a manual state, including repeated
values. Fresh velocity-only receipts correct velocity without rewinding the
predicted position to an older pose. Stale receipts do not clear a newer manual
state. A replacement character resets the old walking target and controller.
Ownership loss discards queued motion; reacquisition cannot replay it.

The bridge uses the existing bounded intent queue and a few scalar predictor
fields. No additional world copy or persistent movement log is retained. Actor
commands now use the [ordered forwarding path](actor-movement.md), including
feedback back into the originating Actor VM.

The existing MoveTo arrival and timeout checks now deliver
[MoveToFinished callbacks and waiters](move-to-completion.md), with a bounded
event queue that survives pose coalescing.

[Local WalkSpeed writes](walk-speed.md) now reach the predictor as ordered
settings and are replaced by fresh received speed observations.

[Partial movement inputs](analog-movement.md) preserve their strength below
one, including commands relative to a pitched camera.

[Local Workspace gravity](local-gravity.md) affects falling and jump-height
calculations while preserving received authority and ordered command handling.

[Direct root CFrame writes and root BasePart pivots](root-transforms.md) reach
the predictor through the existing character-pivot command and feedback path.
BulkMoveTo batches and welded-member CFrame writes also forward their resolved
root pose when they actually move the current local character root.

[MoveDirection and GetMoveVelocity](move-observations.md) now expose intended
walking input separately from physical velocity, with immediate Move updates and
sequence-checked prediction feedback for MoveTo.

Verification uses the official Luau VM, compact shared received fixtures, local
prediction, feedback, and the existing orbit regression. This is not a fresh
live-server, native Roblox, Windows, or RSS result. No server acceptance is
inferred from a locally predicted or encoded frame.
