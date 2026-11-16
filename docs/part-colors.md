# Linked part colors

For BasePart instances, reading `BrickColor` now derives the nearest named color
from the known `Color`. Assigning `BrickColor` writes its palette RGB through
the normal `Color` setter. Clones therefore retain the actual color, and Actor
replay uses one revision-checked Color mutation. `BrickColorValue.Value` and
other unrelated value objects keep their own behavior.

The conversion reuses the pinned BrickColor palette and the existing sum of
absolute channel distances, matching the documented
[BrickColor constructor](https://create.roblox.com/docs/reference/engine/datatypes/BrickColor).
The relationship between the two part properties is described in the
[BasePart reference](https://create.roblox.com/docs/reference/engine/classes/BasePart).
The reset/default APIs derive BrickColor's default from the existing constructor
Color default. Missing received color data stays unavailable; a received
BrickColor alone does not invent an exact Color3.

Local writes and received public or wire color changes notify BrickColor
listeners when the closest palette entry changes. A small RGB change that stays
within the same palette entry still changes Color but does not produce another
BrickColor event. Root and Actor VMs share the comparison logic. Generic Changed
listeners can therefore receive BrickColor in addition to Color; callback order
between different properties is not a native scheduling guarantee.

No extra BrickColor override or persistent per-part color cache is stored.
Palette comparisons for notifications are skipped when neither Changed nor the
specific BrickColor signal has receivers. The ordinary property planner keeps
borrowed input values, and movement-only receipts do not scan color listeners.

Verification uses authored offline fixtures with the official Luau VM, including
received subclasses, Actor writes, clone/reset behavior, unknown data, invalid
local writes, waiters, and palette-stable color changes. No Studio instance was
connected for native comparison. Fresh game-server behavior, Windows execution,
and peak process RAM are not established by this pass.

The three-channel BrickColor constructor accepts mixed integral and fractional
numbers, matching its documented numeric arguments. Regression coverage compares
125 RGB combinations and all 128 palette colors against the Color3 overload.
