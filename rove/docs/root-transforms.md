# Explicit character root transforms

Assigning CFrame on the current local character's root now emits the same
bounded PivotTo command used by a character Model:PivotTo. The ordinary local
pose transaction still runs, including connected rigid-part updates. A full
command queue or invalid root transform fails before that transaction changes
the world. Supported root frames require finite coordinates below one billion
in magnitude and a complete, right-handed orthonormal rotation.

Calling PivotTo on the root BasePart also forwards its resulting root frame.
The command uses the actual CFrame after accounting for PivotOffset, rather
than incorrectly treating the requested pivot as the part's origin.

Workspace:BulkMoveTo now forwards one command for the final root pose if its
resolved rigid-component writes include the current character root. This also
handles a CFrame assignment on a welded member that indirectly moves the root.
The existing last-endpoint-wins rule resolves conflicting requests within a
component before the root command is prepared. Unrelated parts remain local;
they never become character movement commands.

Root lookup happens once for the batch, followed by a scan of the already-bounded
prepared writes. Root coordinates and movement capacity are checked before any
part in the batch changes. A full queue therefore cannot leave unrelated parts
moved while rejecting the root. Batches with no root change need no movement
capacity, and a complete no-op emits no root command. Existing event modes and
Actor bulk-pose fingerprints remain in use.

Position, Orientation, and Rotation assignments on the current root also
forward the resulting frame. They keep the existing single-part setter
behavior: neighboring parts stay in place, and incident joint offsets are
rebased to match. Movement validation and queue capacity are checked before
either the part or its joint offsets change. Identical assignments emit no
command, and edits to other parts need no character movement capacity.

Actor root transforms are captured with their pose journal and forwarded once
after it merges. Journal replay suppresses duplicate commands. Both root paths
then use the predictor's existing received player, character, root, ownership,
anchoring, and streaming checks. Losing authority discards commands rather than
replaying them when authority returns. A valid transform cancels an earlier
MoveTo, while a later request remains active.

The received world is not rewritten by a script's transform. Accepted local
prediction feeds position and rotation back into Luau; this is not evidence
that a game server accepted the movement. No new world copy or persistent pose
history is added.

Tests cover root CFrame prediction and feedback, rotation, MoveTo cancellation,
BasePart pivot offsets, Actor single delivery, ownership and anchoring rejection,
and validation/queue failure before a connected assembly moves. Batch tests
exercise indirect welded movement, conflicting endpoints, atomic rejection,
no-op handling, unrelated parts, and Actor forwarding. Existing bulk move and
orbit regressions cover related pose transactions.

Alias tests cover all three setters, joint offsets through prediction feedback,
Actor delivery without duplicates, and queue failures without partial changes.
Full native articulation parity,
fresh live-server acceptance, Windows execution, and RSS measurements remain
separate verification work.
