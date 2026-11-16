# Actor reads after received updates

Actor synchronization previously merged old script overrides back on top of the
root world after a received update had removed them. The root could report a new
position, color, or health while an Actor still read the old value, including
inside its property-change callback.

Synchronization now prepares notifications against the old Actor state, clears
overrides touched by accepted received property actions, and adopts the root's
committed world before publishing callbacks. Root and Actor invalidation share
the same rules for public wire aliases, BasePart frame aliases, Attachment pose
aliases, and Humanoid state feedback. Name and Parent overrides are also cleared
when those properties are received.

Only receipts newer than the Actor's received sequence and no newer than the
committed root sequence are eligible. A local mutation journal is not a received
update. Unrelated local overrides survive; subsequent script writes remain
possible; retrying an old receipt cannot discard those newer writes. Rejected
transactions never enter the accepted-receipt path.

The refresh scans hierarchy headers rather than expanding every shared record.
Invalidation only accesses nodes that already have materialized local overrides.
There is no additional persistent per-instance revision map or receipt queue.

Regression coverage includes root-authored and Actor-authored overrides,
callback reads, identical wire values, stale retries, rejected batches, names,
parents, Attachment transforms, and the compact shared-world projection path.
A separate 1,000-part test checks that synchronization leaves untouched received
records unmaterialized. This is a structural allocation check, not a live RSS
benchmark. Offline Luau tests do not establish fresh server acceptance or Windows
execution.
