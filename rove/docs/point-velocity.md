# Point velocity queries

The Rust host now implements BasePart:GetVelocityAtPosition(position), including
inherited BasePart classes. It returns the current assembly's velocity at a
world-space point using `linear + angular × (point - centerOfMass)`. See the
[Roblox BasePart reference](https://create.roblox.com/docs/reference/engine/classes/BasePart#GetVelocityAtPosition).

Received parts use their assembly observations. Locally created rigid members
use the existing assembly-root selection and center-of-mass calculation. The
query reads current values each time, including shared received properties and
new observations delivered to Actor scripts. It does not author a movement
mutation or cache a second copy of the world.

Missing linear or angular velocity remains an error. The center of mass is only
needed when angular velocity is nonzero. Input must be a finite Vector3, the
receiver must be a live BasePart, and results outside finite Vector3 range are
rejected. These rules preserve unknown received state instead of inventing a
stationary assembly.

Character physics feedback updates AssemblyLinearVelocity in the root and
existing Actor VMs without emitting BasePart property-change signals. Scripts
can query the value during simulation callbacks. This corrects the earlier
notification behavior to match the documented physics exception.

Coverage includes linear and rotational motion, all three cross-product axes,
local welded assemblies, unknown inputs, overflow, destroyed receivers, Actor
callbacks, shared received updates, and a 1,000-part lazy-storage check. Tests use
the official Luau VM on macOS. Fresh live-server verification, native Roblox
behavior comparison, Windows execution, and RSS measurements are separate work.
