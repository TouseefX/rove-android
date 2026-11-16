# MoveTo completion events

The local character predictor reports an arrival as `MoveToFinished(true)` and
an eight-second timeout as `MoveToFinished(false)`. These results reach both
ordinary Luau callbacks and signal waiters, including those in live Actor VMs.
The behavior follows the [documented event contract](https://create.roblox.com/docs/reference/engine/classes/Humanoid#MoveToFinished).

Each result carries its originating movement sequence. The feedback transport
deduplicates repeated observations and retains discrete completion results
separately from its two coalesced pose slots. Delivery waits for the corresponding
world revision. Replacing intermediate poses therefore cannot erase intermediate
completion events. Events for a replaced Humanoid are discarded instead of being
sent to the replacement character.

At most 256 undelivered completion results are retained. Overflow is an explicit
error before the new result or pose is accepted; existing results remain queued.
After draining, the rejected result can be submitted again. The runtime reports
the pending count as `physics_move_to_events_pending`. No complete world copy or
unbounded event history is added.

The Luau feedback handler validates all result rows before applying observations.
Rows require increasing positive sequences and boolean results. Each VM tracks
its own delivered sequence on the Humanoid, so repeated feedback does not fire
the event twice. Root and Actor callbacks receive the same ordered booleans.

`MoveTo(location, part)` now retains a BasePart reference and transforms the
offset by that part's current received position and rotation on each prediction
tick. The newest received CFrame or ReplicationPV supplies the pose. Omitting
the part, or passing nil, restores a world-space destination. The method also
writes WalkToPoint and WalkToPart so Luau can inspect the requested destination.
It validates arguments and queue capacity before these property writes. Actor
commands carry the part reference through the existing ordered merge path.

For received targets the predictor requires a BasePart inside the character's
received Workspace with a valid pose. Missing, removed, reparented-out, or malformed targets
stop steering; the original eight-second deadline keeps running. A usable target
can resume steering before that deadline. This is a conservative unavailable-data
policy, not a verified claim about native destruction behavior. One bounded part
identifier is retained; no target-world clone or per-tick node scan is added.

Local target edits now reach steering too. After script work and Actor merges,
the runtime samples the one active target and sends a changed pose through the
bounded movement queue. Locally created BaseParts are supported when their local
ancestry reaches the character's Workspace. Translation and rotation both affect
the destination. Local detachment or destruction sends an unavailable observation
and stops steering; receiving a new pose alone cannot reattach that target.

Each observation identifies its original MoveTo sequence and target. It cannot
restart the eight-second deadline, revive a canceled request, or change character
ownership. A newer received pose supersedes an older local pose, including an
equal-value receipt. Scripts may intentionally assign the local pose again after
that receipt. Unchanged observations are suppressed; a full queue defers sampling
until capacity returns and then forwards the latest target state. There is one
bounded watcher and one pose, without a world copy or history of target motion.
Received targets still require valid received ancestry; locally reparenting a
received part from a different Workspace is not a substitute for that evidence.

Changing WalkToPoint or WalkToPart on the local Humanoid now queues an ordered
cancellation. The assignment alone does not start a new MoveTo, and assigning
the same value does not cancel or reset its deadline. A later MoveTo can start a
new request in the same command batch. Manual Move input is unaffected when no
MoveTo is active. Values and movement capacity are checked before the property
write; edits on other Humanoids remain local property changes.

Actor destination edits follow the same merge and forwarding path, without
generating another cancellation when their property journals replay. A valid
character Model:PivotTo cancels earlier MoveTo requests while preserving a later
request in the same batch. A pivot rejected by the received-root check cannot
cancel the active request. Canceled requests no longer generate a later timeout;
this patch does not assert native parity for cancellation event timing.

This does not add pathfinding, general NPC simulation, or native parity for all
cancellation conditions. [Direct root CFrame assignments](root-transforms.md)
now use the same cancellation path as accepted character pivots. Received
destination property changes remain a gap in this predictor's MoveTo handling. Arrival still
uses the existing horizontal threshold. An arrival is a locally predicted result,
not proof that the server accepted the movement. Live Roblox, Windows execution
and peak-RAM verification are separate gates.
