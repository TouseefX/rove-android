# Actor movement commands

Actor movement commands now join the root movement queue after their local
mutation journal is merged. This covers Move, MoveTo, Jump, ChangeState,
SetStateEnabled, PivotTo, AssemblyLinearVelocity, WalkSpeed, Gravity, and
cancellation from WalkToPoint/WalkToPart edits. The existing predictor
still determines which commands and states it implements and checks received
ownership before moving a character.

The root assigns one increasing sequence across root and Actor commands. Actor
order follows the existing scheduler's rotating visit order; commands within an
Actor keep their captured order. Each forwarded row retains actor_origin and
actor_sequence for diagnostics. Its mutation_cursor maps to the corresponding
committed root-journal boundary, including commands with no property mutation.

Replaying an Actor property journal suppresses newly generated movement commands.
The captured command is forwarded once instead. The suppression guard is scoped
and is removed even when a merge fails. This prevents replayed Jump and velocity
setters from duplicating commands or changing their original order.

When a journal row conflicts, commands after that row are rejected conservatively;
commands captured before the conflict can still be forwarded. A failed merge
rejects that Actor batch's captured commands. Rejections appear in the Actor
status's movement_forwarding report. Commands retain captured camera, jump and
velocity parameters rather than reconstructing them from later world state.

Before each Actor advances, its movement allowance is set to the root queue's
remaining capacity. The shared limit is 256 commands per response. A full queue
raises an error in the Actor's original method call, where pcall can catch it.
Jump, velocity and pivot preflight this limit before changing properties. An
Actor receives a fresh allowance on its next turn. This avoids a persistent
backlog and bounds both queue storage and merge tracing; tracing is allocated
only for Actor journals that include movement commands.

The VM owner and receive-thread queues each retain at most 512 commands. Each
physics step drains at most 256, leaving any tail for the next step. Transfers
check destination capacity before draining the VM queue, so temporary saturation
keeps accepted commands in order instead of removing them before an overflow
error. This is bounded buffering, not unlimited throughput: a producer that
continues filling the VM owner's 512-command queue still gets an explicit queue
limit error. Commands are not coalesced, because intermediate jumps and state
changes can affect later commands.

Tests run the official Luau VM against compact shared fixtures. Coverage includes
multiple Actors mixed with root commands, ordered single delivery, local physics
and feedback back into an Actor, pivot/jump replay, queue exhaustion and recovery,
mutation conflicts, suppression cleanup after errors, a 400-command prediction
burst and a 600-command transfer through a saturated queue. The existing authority
checks, static-AABB solver and supported state subset remain unchanged. There is
no fresh live-server, native Roblox, Windows, or peak-RAM result.
