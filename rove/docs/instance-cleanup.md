# Instance cleanup

`Instance:ClearAllChildren()` destroys each child and its descendants while
leaving the container usable. It uses the same local destruction implementation
as `Instance:Destroy()`. See the official [Instance reference](https://create.roblox.com/docs/reference/engine/classes/Instance).

`Debris:AddItem()` now uses that implementation too, both when an item's lifetime
expires and when the host's 1,000-item queue evicts its oldest entry. The earlier
Debris shortcut only marked nodes destroyed and emitted a root-ID mutation. It
skipped connections, waiters, events, and the revision fields needed to merge
Actor mutations.

The shared path now:

- Queues `Destroying` and hierarchy removal events for the complete subtree,
  preserving old ancestry until all descendant notifications are queued.
- Disconnects instance connections and releases child waits, signal waits, and
  pending bindable/remote calls through their existing cleanup handlers.
- Detaches and marks all selected nodes destroyed, preventing reparenting.
- Emits the complete destroy mutation, including Actor revision fields.
- Treats repeated destruction as a no-op.

Debris runs in the host and Actor pumps independently of the script that queued
it. Empty queues are released, and an unused Debris service creates no queue in
the tick path. Destruction errors are returned to the pump; other items due in
the same tick are still processed. The existing 100,000-node limit on a single
destroyed subtree remains in place.

Signal callbacks still use the runtime's deferred delivery. These changes do not
implement the full choice of Roblox `SignalBehavior` modes or claim complete
native event-order equivalence. Local unit and command-loop tests use authored
fixtures, including Actor-to-root journal replay; they do not connect to a live
Roblox server.
