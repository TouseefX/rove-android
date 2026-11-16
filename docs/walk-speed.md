# Local WalkSpeed changes

Writing the current local character's Humanoid.WalkSpeed now queues a movement
setting alongside Move, MoveTo, state changes and velocity writes. The predictor
uses the captured speed after validating the current character and received
ownership. Setting zero stops the normal horizontal walk controller. Physics
state still bypasses that controller, so WalkSpeed does not erase an explicitly
assigned free-motion velocity.

The setter accepts finite numbers from 0 through 10000, matching the existing
predictor's supported range. Type, range and queue-capacity checks happen before
the property changes. Other Humanoids retain ordinary local property writes;
this feature does not simulate their movement.

The speed persists across empty ticks, unrelated received updates and pose
corrections. A fresh received WalkSpeed observation supersedes it, even when the
received number repeats an older value. Sparse per-property revision metadata
makes that distinction without retaining a second world. An intent records the
VM's received sequence, so delayed commands from before that observation cannot
restore an obsolete speed. A later script write can override the new observation.

Replacing the character or Humanoid, or losing and then regaining received
ownership, resets the predictor to received settings. Actor writes follow the
same ordered merge and replay-suppression path as other movement commands.

The documented meaning of WalkSpeed and zero-speed behavior comes from the
[Humanoid API](https://create.roblox.com/docs/reference/engine/classes/Humanoid#WalkSpeed).
Tests use the official Luau VM and compact shared world fixtures. They do not
establish fresh live-server acceptance, native controller parity, Windows
execution, or a peak-RAM measurement. Workspace.Gravity uses its separate
[persistent Workspace setting](local-gravity.md).
