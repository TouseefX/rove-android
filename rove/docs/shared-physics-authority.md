# Physics checks against shared world data

Owned-body feedback now reads ownership, anchoring, assembly-root references,
Player.Character, and joint/constraint references through the common world
property reader. That reader handles local overrides, compact shared received
records, and ordinary snapshot properties consistently.

Previously these checks read the older property map directly. A compact record
could therefore lose its owner from the check and reject a valid update. When a
local owner override was present, the same mistake could hide received anchoring,
character membership, or a connection and incorrectly accept an isolated-body
update. Some reference checks also preferred an old received value over a newer
local override.

The change preserves the isolated-body channel's existing limits and checks:
the body must have the expected owner, live under Workspace, be unanchored, and
be outside player characters and joint/constraint assemblies. Unknown ownership
is not treated as local ownership. Source-revision fences remain in place, and
the entire batch is validated before any body is moved.

Regression tests exercise both accepted and rejected shared-world feedback,
local reference additions/removals in both storage forms, and a mixed-authority
batch. The Luau integration test accepts feedback using shared received ownership
and rejects it after a received owner change. A 1,000-part fixture checks that
unrelated shared parts remain unmaterialized; this is not a live RSS benchmark.

The tests use the official Luau VM and local fixtures on macOS. They do not prove
fresh live-server acceptance or Windows execution. This change does not broaden
the isolated-body channel to other part classes or connected assemblies.
