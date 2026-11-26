# Property notifications

Scalar received property updates now preserve notification order within a
transaction. Changing a value and then restoring it produces both transitions.
Equal consecutive values and stale retries produce no duplicates, and rejected
transactions publish nothing. The explicit `property` or `key` field overrides
the same key in `fields`, matching the world transaction itself.

`ValueBase.Changed` fires only for `Value` and passes the new typed value. Other
properties on value instances still notify their `GetPropertyChangedSignal`
subscribers. Ordinary `Changed` callbacks receive a property name, and specific
property callbacks receive no arguments, following the
[Object reference](https://create.roblox.com/docs/reference/engine/classes/Object).

The root VM and Actor frame synchronization share the ordered planner. Actor
receipts preserve intermediate values across received and local batches.
`ValueBase` event payloads capture each transition; callbacks that read a property
directly still see committed state because signal delivery is deferred.

Received `Color3uint8` and `CollisionGroupReplicate` updates notify the public
`Color` and `CollisionGroup` properties. The planner compares decoded values:
changing a collision-group ID without changing its name produces no public
notification. Receiving the existing wire value can still produce a notification
when it clears a different local public override. An explicit stored public
property retains its existing precedence over the wire fallback, so a masked
wire update produces no public event. A row containing both names produces at
most one public notification. Raw transport field names are not added to the
received public `Changed` event stream. The public properties are described in
the [BasePart reference](https://create.roblox.com/docs/reference/engine/classes/BasePart).

The planner reads old values for touched keys and borrows incoming values for
comparison and event payloads, rather than copying every target's property map.
Value payloads are converted to Luau only when an enabled callback or waiter
receives them.
Received scalar and BasePart transform notifications no longer require a full
old-node snapshot. Received CFrame rows derive Position, Orientation, and Rotation
using the same conversion as property reads. Intermediate transforms survive a
batch that returns to its initial pose, and Actor VMs receive the same derived
notifications. Equal poses, stale retries, and rejected transactions stay silent.
Explicit aliases in the same received row are subordinate to its canonical frame,
matching public read precedence. Translation alone does not notify rotation.
Session forwarding borrows
received actions until Actor synchronization; it does not clone their payloads
when no Actor VM exists.

Hierarchy, Player.Character, attachment transforms, and joint poses retain their
specialized notification paths. Physics-driven signal frequency, native immediate
callback scheduling, and aliases beyond those named above are not established
by these tests. Shared-world integration also exercises normalized receipts and
projected records through the official Luau VM. This remains offline verification,
not fresh game-server or Windows execution.
