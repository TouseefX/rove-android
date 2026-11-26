# Assembly angular conversions

The Rust host implements `BasePart:AngularAccelerationToTorque(acceleration, angularVelocity?)`
and `BasePart:TorqueToAngularAcceleration(torque, angularVelocity?)`.
The calculation follows the [BasePart API](https://create.roblox.com/docs/reference/engine/classes/BasePart#AngularAccelerationToTorque):
`torque = I * acceleration + angularVelocity × (I * angularVelocity)`.
The inverse subtracts the gyroscopic term and solves the positive-definite tensor.
Omitted angular velocity is zero; it does not implicitly read assembly velocity.

The current rigid graph determines the assembly and root. Massless members other
than the root contribute no inertia. Each included part uses the current mass
resolution, including received observations and material dependency freshness.
Primitive geometry supplies the center and covariance: blocks, balls, cylinders,
wedges, and corner wedges. Rotated local tensors and the parallel-axis theorem
combine them about the assembly's mass-weighted center. Wedges retain off-diagonal
terms. Balls use their smallest dimension; cylinders use the smaller transverse
dimension, consistent with this runtime's primitive mass rules.

Anchored assemblies and parts without a Workspace physics container return an
infinite torque vector or zero angular acceleration. Destroyed receivers, invalid
arguments, unknown mass or geometry, and nonfinite results are errors. Mesh and
union inertia are not inferred from a bounding box or render mesh. WorldModel
physics behavior and exact native edge-case agreement remain unverified.

Queries read the current world each time. They do not set properties, apply a
force, enqueue outgoing movement, or retain tensor caches. Accumulation stores one
member's tensor at a time, with a maximum of 4096 members. Existing graph traversal
also applies its own limits. Online centered accumulation avoids cancellation
from subtracting large world-origin moments; inverse solves use a scaled Cholesky
factorization. This is not a measured peak-RAM claim.

Verification uses synthetic received updates and authored Luau scripts in the
official VM. Fresh live-server behavior, native Roblox comparisons, Windows
execution, mesh inertia, and full rigid-body simulation remain separate work.
