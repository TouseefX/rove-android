# Local physics observations in Actor scripts

The character-pose and owned-body feedback adapters now forward validated engine
observations to existing Actor VMs. Previously, a root script could read the new
position while an Actor retained an older script override.

Each adapter runs its existing validation and commit path, then forwards the
committed fields to existing Actors. Physics-driven BasePart changes deliberately
emit neither Changed nor GetPropertyChangedSignal. This follows the documented
[Object signal limitations](https://create.roblox.com/docs/reference/engine/classes/Object#GetPropertyChangedSignal).
Scripts can read the current pose from simulation callbacks or ordinary queries.
The whole batch is written before dependent attachment and joint observations
are refreshed. This corrects an earlier implementation and test expectation that
emitted BasePart property signals for physics feedback.

Character updates cover CFrame, Position, Orientation, and Rotation. Owned-body
updates also cover AssemblyLinearVelocity and AssemblyAngularVelocity. The
existing character, ownership, source-revision, and batch-validation checks remain
in force. Rejected or stale batches publish no pose notifications. Engine
observations use the engine property path and do not become script-authored
movement mutations.

Observation storage is temporary and bounded by the existing 32-part batch limit.
The adapter visits existing Actor VMs directly; it does not add a persistent
receipt queue or copy the full world. Actors that are created later inherit the
current root world through normal initialization.

Tests use the official Luau VM, synthetic engine feedback, and the offline
400-step moving-target orbit. They cover existing Actor overrides, silent physics updates, repeated observations, multi-part visibility,
attachment updates, owned-body velocity, and stale/rejected input. These checks
do not establish live-server acceptance, Windows execution, exact native physics
signal scheduling, or a measured RAM reduction. The session's combined pose and
Humanoid-feedback command remains two separately validated operations.
