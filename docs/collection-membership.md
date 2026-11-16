# CollectionService membership

`GetInstanceAddedSignal(tag)` and `GetInstanceRemovedSignal(tag)` now follow
known tagged instances entering and leaving the DataModel, including a tagged
child whose ancestor moves. Moving between two parents already inside the
DataModel produces neither notification. Destruction, `ClearAllChildren()`,
and Debris cleanup notify subscribers once for each affected tagged instance.
This follows the [CollectionService reference](https://create.roblox.com/docs/reference/engine/classes/CollectionService).

Received tag snapshots, `set_tags`, creates, parent changes, and removals use an
ordered notification plan prepared against the old world. The plan is published
only after the received transaction validates. Duplicate or stale receipts and
rejected transactions publish nothing. Detached tagged instances produce no
membership notifications until they enter the DataModel.

The plan carries only tag strings, IDs, and changed parent links. It does not
retain a second world index. Property-only movement bypasses the tag scan, and
ancestry checks read received headers without allocating property wrappers.
Actor VMs receive the ordered receipt through their existing frame sync, before
local mutations from that frame, rather than performing a second world copy.

Tests cover local writes, received batches, rollback, stale updates, destruction,
Actor-to-root and root-to-Actor changes, and a root callback destroying a newly
received tagged part. A shared-record test checks that 1,000 unrelated records
stay unmaterialized; this is not a process RAM benchmark.

These are authored offline fixtures running the official Luau VM. They do not
establish fresh server acceptance or Windows execution. The runtime still uses
deferred signal delivery. Unknown received tag state stays unavailable rather
than being reported as an invented empty tag set. Global `TagAdded`/`TagRemoved`
and the newer Collection query API are outside this change.

The destruction regression also exposed a separate Actor merge bug: a waiter
completed inside its VM, but its attribute write was rejected as a false revision
conflict. Scalar property, attribute, and tag journal rows now carry their result
revision privately. Replay translates revisions only after accepting the write
that produced them, so later writes in that batch follow the accepted root
version. The final accepted node revision is carried back to the fork for its
next frame. A competing root write still fails the original fence. Internal
revision fields are stripped from session output.

Actor replay also handles a local instance created and destroyed within the same
frame. The final source tombstone is accepted for replay only when a later
destroy row identifies that instance with its matching final revision. Creation
and intervening scalar writes still undergo namespace, class, ownership, and
root revision checks. This preserves both tag notifications for a short-lived
instance; a destroyed source without the matching later destroy is rejected.
