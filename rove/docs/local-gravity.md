# Local Workspace gravity

A script write to Workspace.Gravity now affects the detached local character
solver, including free fall and JumpHeight-based jumps. Gravity commands share
the ordered movement queue with jumps and velocity writes. For example, a jump
between two gravity changes uses the first value for its launch calculation and
the second value for subsequent falling.

The setting belongs to Workspace rather than a particular Humanoid. It can be
accepted while the local player has no character, survives respawn, and remains
available after ownership is lost and reacquired. None of those cases permit
simulation without the existing received ownership, anchoring, streaming and
character checks. Settings do not carry into a different session scope or
Workspace.

Only one local gravity setting is retained. It records the received Workspace
value and its sparse property revision. A fresh server gravity receipt replaces
it, even if the number is unchanged. Queued writes captured before that receipt
are rejected; a later script write may set a new local value. Unrelated received
changes do not erase the setting.

Actor writes merge into the same command order. Journal replay suppresses extra
commands. Type, finite-range and queue-capacity validation occurs before the
local property changes. The supported solver range is 0 through 1000000;
negative gravity is not implemented by this solver.

This connects the [Workspace Gravity property](https://create.roblox.com/docs/reference/engine/classes/Workspace#Gravity)
to the existing character predictor, not to a general rigid-body world. Received
world values remain unchanged. Feedback exposes the predicted character motion,
and reports include active gravity and the accepted-gravity-write count.

Tests cover real Luau and Actor commands, fall position and velocity, feedback,
pre-character settings, respawn, ownership loss, ordered jumps, server
corrections, stale queued writes, invalid targets, session changes and queue
exhaustion. Live server acceptance, native Roblox parity, Windows execution and
peak RAM have not been verified for this change.
