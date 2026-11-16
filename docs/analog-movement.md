# Partial movement input

The horizontal movement controller now preserves Move input strength below one.
At WalkSpeed 32, an X input of 0.25 requests 8 studs per second instead of 32.
Vectors longer than one are capped, so diagonal input cannot exceed WalkSpeed.
Vertical input remains ignored by this walking controller. The same rule is used
by the standalone movement integrator.

Camera-relative input uses the captured camera's horizontal heading. Its pitch
does not reduce input strength. When looking vertically, the captured right axis
provides a horizontal heading. The frame is still validated before use, and
changing CurrentCamera after submitting a command does not change that command.

Humanoid and Player Move calls, including Actor commands, use this path. Existing
Physics-state and explicit-velocity handling are unchanged. MoveTo still uses
full walking speed outside its arrival threshold.

Roblox documents slower movement for partial thumbstick input in the
[WalkSpeed notes](https://create.roblox.com/docs/reference/engine/classes/Humanoid#WalkSpeed).
This implementation preserves that input strength in the current local solver;
it does not establish native parity for controller acceleration, unusual camera
roll, swimming, flight, or moving platforms.

Verification covers real Luau commands through the shared-world predictor,
actual position/velocity results, Running-speed feedback, Actor ordering,
captured camera orientation, vertical-camera fallback, and fixed-step local
integration. No Studio instance was connected for native comparison. There is
no fresh live-server, Windows-execution, or peak-RAM result.
