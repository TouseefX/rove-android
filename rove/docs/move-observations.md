# Intended Humanoid movement

Humanoid:Move and Player:Move now update MoveDirection immediately after their
command is accepted. Direction is a world-space unit vector, or zero for stopped
input. GetMoveVelocity preserves partial input strength and multiplies it by the
current supported WalkSpeed. Changing WalkSpeed therefore changes this intended
velocity without requiring another Move call. These follow the documented
[MoveDirection](https://create.roblox.com/docs/reference/engine/classes/Humanoid#MoveDirection)
and [GetMoveVelocity](https://create.roblox.com/docs/reference/engine/classes/Humanoid#GetMoveVelocity)
contracts within the existing horizontal walking model.

Camera-relative input uses the same captured camera transform as prediction.
Later camera edits do not rotate an already-issued command. A rejected command,
including queue exhaustion, cannot overwrite the previous input observation.
MoveDirection is read-only to scripts. Direction property callbacks fire only
when the direction changes; changing analog strength along the same direction
changes GetMoveVelocity without inventing a direction change.

Prediction feedback supplies input for MoveTo steering, arrival, cancellation,
and unavailable-target stops. It carries the consumed movement sequence. Feedback
older than a newer accepted Move cannot overwrite that newer input observation.
Input is validated before any state, speed, or velocity observation is committed.
The existing pose-plus-feedback operation remains a separate transaction boundary.

Intended movement is not derived from physical velocity: a character can request
movement while stopped by the controller or moving under explicit Physics-state
velocity. Actor commands update their own observations and, once accepted, the
root observations in global command order. Root-owned input and sequence fields
cannot be replaced by stale Actor overlays during synchronization.

The implementation retains two small vectors and one sequence per observed
Humanoid. It adds no world clone or new feedback queue. Unknown initial input is
not inferred from a nonzero MoveDirection alone; a known zero direction can yield
zero intended velocity without needing a speed. MoveTo observations update on the
prediction tick. Swimming, flight, full native scheduling/clone parity, live-server
acceptance, Windows execution, and RSS benchmarks remain separate work.
