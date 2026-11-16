# received part movement

With physics enabled, authored CFrame and transform-alias writes can now queue
physics records for received, locally owned, isolated BaseParts. BasePart:PivotTo
and BulkMoveTo use the same pose path. The own-character controller keeps its
existing sender. Actor-produced isolated-part commands are not yet forwarded.

The VM carries the received creation sequence, world sequence and network owner
with each pose. Locally created parts have no such authority. Before forwarding
effects, a later local anchor, reparent, destruction or joint attachment cancels
pending poses. The received-world owner checks ownership, generation, anchoring,
ancestry, assembly connections and newer receipts again. Other player characters
are excluded.

The sender uses the received class's ISR property table and the negotiated
legacy PV compressor on application 4, channel 5. A local pose-scheduler clock
advances from this session's monotonic intervals. The remote clock offset stays
unknown until there is real delivery-history evidence. Records contain PV only;
they do not fabricate a Humanoid state, root binding, or delivery acknowledgment.
At most 64 authored parts are active. New writes coalesce before the next physics
step, and packets contain at most eight records. A bounded ordered queue keeps
physical poses together with their contact transitions; saturation or a batch
older than 250 ms fails explicitly instead of losing contact history.

Velocities come from decoded or compact received PV fields. A complete received
creation without PV may use the pinned constructor's initial velocities; reports
label that source `local_constructor_initial_state`. Missing, malformed or
unsupported velocity data remains unavailable.

The isolated-box model follows the Python client's `owned_part_dynamics.py`,
`owned_part_simulation.py`, and `owned_part_contacts.py`: elapsed steps are capped
at 250 ms and subdivided at 240 Hz. Gravity and one capped BodyVelocity preserve
momentum across teleports. Static box collisions are inelastic and frictionless.
Bounded initial-overlap correction uses the Python model's stable minimum-axis
choice. Unsupported rotation, angular motion, nearby dynamic or non-box geometry,
non-default collision groups, unknown material mass, and other force classes
remain explicit limitations. This is not Roblox solver parity.

Only a completed supported physical step can produce contact begins/ends against
received anchored Part listeners with TouchTransmitters. Detection uses oriented
boxes and current ownership, creation, CanTouch and Workspace facts. Teleport-only
frames do not produce touches. Loss of authority clears history without inventing
a TouchEnded. Contacts use application 4, reliable channel 7, after the
corresponding pose submission. A physical phase that starts overlapping and
separates during depenetration can emit both begin and end transitions; checking
only the final pose loses that real transient contact.

Python's `owned-contacts-validation.json` and
`wire-v13-cp9-callback-correlation.json` document the earlier successful CP9 path.
They are not evidence that this Rust port passes a live test.

Local anchor/reparent/destroy/joint edits stop previously tracked bodies as well
as pending writes. Local size, collision, material, velocity-controller and
gravity overrides not yet ported to this model also stop it. Predicted
isolated-body feedback to Luau remains separate work; the sender does not
overwrite the authoritative received world.

`owned_part_physics` reports authored writes, prepared records, actual send
returns, coalescing, rejection reasons, velocity sources and dropped records.
A send return is not proof of server acceptance. Unit fixtures compare bytes
against the independent Python codecs and Rust ISR decoder, and compare physics
results against the Python model. Runtime tests exercise actual Luau writes and
later cancellation through the shared received-world bridge. Live suite
completion must be verified separately.
