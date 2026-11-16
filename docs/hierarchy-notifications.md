# Hierarchy notifications

Moving a subtree now sends `DescendantRemoving` and `DescendantAdded` for each
affected descendant. Ancestors shared by the old and new parent receive neither
event. Descendant removals precede `ChildRemoved`, with the moved root first.
`AncestryChanged` reaches listeners on descendants and supplies the instance
whose parent changed, followed by its new parent. `Parent` property listeners
also receive each actual transition.

The behavior follows the [Instance reference](https://create.roblox.com/docs/reference/engine/classes/Instance).
Local changes, received transactions, and Actor synchronization use the same
notification planner. Received batches preserve intermediate parent changes;
stale retries and rejected transactions publish no notifications. Children
received before their parent are announced to the new ancestors when that
parent arrives. Destruction removes each descendant once from external
ancestors and retains cleanup for signals, bindables, and remote callbacks.

The planner reads headers and records changed parent links. It builds a temporary
child index only when descendant listeners or destruction require traversal.
Movement-only receipts skip planning. Destruction notifications no longer need
a second snapshot of descendant properties. A 1,000-part shared-world fixture
checks that planning does not materialize their property wrappers; this is not
a measurement of peak process memory.

`rust-client/tests/hierarchy_runtime.rs` exercises the real command loop and
official Luau VM, including Actor callbacks, ancestry waiters, orphaned children,
ordered batches, duplicate receipts, and transaction rejection. These are
offline fixtures. No fresh game-server session or Windows execution is claimed.
Signal delivery remains deferred: callbacks read committed world state, while
event arguments retain the intermediate parent changes. Full native immediate
callback and destruction scheduling parity is outside this change.
