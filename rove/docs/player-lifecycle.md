# Player and character lifecycle

The Rust host now derives `Players.PlayerAdded`, `Players.PlayerRemoving`,
`Player.CharacterAdded`, and `Player.CharacterRemoving` from committed world
updates. Scripts can wait for a character and then its parts:

```lua
local player = game:GetService("Players").LocalPlayer
local character = player.Character or player.CharacterAdded:Wait()
local root = character:WaitForChild("HumanoidRootPart", 10)
if root then print(root.Position) end
```

- Existing snapshot members are initialized silently. Scripts should enumerate
  `GetPlayers()` and check `Character` when attaching to an existing session.
- A Character reference can precede its Model. The host waits for the real Model
  and cancels that pending notification if the reference changes or the Player is
  removed. It does not require the Model to have a parent, Humanoid, or root part.
- Respawns preserve removal/addition order, including several changes in one
  receipt. Repeated values, repeated full records, and stale receipt sequences
  do not cause extra lifecycle events.
- Rejected world transactions publish no lifecycle effects. The event plan and
  reference tracking commit only after the world update succeeds.
- A reflected lifecycle event accompanying its state change is delivered once.
  Its supplied arguments are retained; departures without a supplied reason use
  `Enum.PlayerExitReason.Unknown`.
- Local writes, membership changes, destruction, and Actor synchronization use
  the same lifecycle rules. Registries and subscriptions remain separate per VM.
- `CharacterAppearanceLoaded` is available as a signal but is not inferred from
  `CharacterAdded`; receiving a Model does not prove its appearance is loaded.

`WaitForChild` checks direct children, returns an existing child immediately, and
otherwise parks the coroutine. Received creation, local reparenting/renaming,
and Actor synchronization can resume it. A supplied timeout returns nil; zero
performs an immediate lookup. Destroying its parent releases a pending wait with
nil. The outstanding waiter limit is 2048 per VM, shared with InstanceHandle
waits. The optional native five-second infinite-yield warning is not implemented.
Cancelled child and InstanceHandle waits cannot be restarted by a later arrival
or timeout. Registration and the shared root/Actor frame loop release cancelled
wait records and their timers, including when no world changes arrive.
Signal waits such as `CharacterAdded:Wait()` also ignore cancelled tasks when
events fire or their source is destroyed. Signal registration and inspection
discard cancelled wait records, so repeated cancellation cannot exhaust the
per-signal connection limit or count as an active waiter.

The lifecycle registry retains only player IDs, parent IDs, and character
references. Ordinary movement receipts skip the lifecycle plan. It retains no
copy of the whole world. Like other host signals, callbacks are scheduled after
the committed update; event arguments preserve transition order while property
reads see the committed world.

The official semantics are documented in Roblox's [Player reference](https://create.roblox.com/docs/reference/engine/classes/Player),
[Players reference](https://create.roblox.com/docs/reference/engine/classes/Players), and
[Instance reference](https://create.roblox.com/docs/reference/engine/classes/Instance#WaitForChild).

## Verification

Local verification on 2026-11-14 passed 129 focused checks, including 12 new
Luau lifecycle/wait tests. A receive-only Jailbreak capture replay applied 9,658
world receipts to the VM and observed 20 PlayerAdded, 4 PlayerRemoving, 30
CharacterAdded, and 14 CharacterRemoving callbacks, with zero observer errors.
The capture contained no explicit lifecycle-event records; these callbacks came
from the received world changes. No captured scripts executed and no packets
were sent. This does not establish a fresh live connection or a Windows run.
The replay started before the final mixed-field precedence correction; that
correction was verified separately by the final Luau regression test.

A second complete replay, including that correction and the root-global/remote
cleanup optimizations, produced the same counts and zero observer errors. Its
debug elapsed time was 774.4 seconds versus 1217.6 seconds previously. This is an
offline replay observation, not a controlled live performance benchmark. The
subsequent waiter-cancellation changes are covered by separate Luau tests and
were not part of that replay binary.

The final cancellation verification passed 135 focused checks: 122 unit checks
and 13 lifecycle/movement integration checks. Regression tests reproduce the
original signal-task restart and connection-limit failures, then verify cancelled
child, InstanceHandle, and signal waits, timer cleanup without world changes,
active callbacks, Actor behavior, and received positions across Luau yields.

`rust-client/tests/player_lifecycle.rs` runs real Luau integration tests covering
spawn waits, delayed references, respawns, rollback, membership, reflected events,
compact shared worlds, Actor isolation, local writes, and child-wait timeouts.
The existing Windows workflow runs all integration tests with `--all-targets`.
A local macOS pass alone does not establish a Windows pass.

`replay_player_lifecycle_capture` is an offline, receive-only replay helper. It
applies decoded receipts to the Rust world and runs a fixed Luau observer. It
does not run captured scripts, send packets, or reuse captured authentication or
integrity data. It reports actual callback counts and observer errors, separately
from fresh live-server verification.

```sh
cargo test --manifest-path rust-client/Cargo.toml --test player_lifecycle
cargo run --manifest-path rust-client/Cargo.toml --example replay_player_lifecycle_capture -- CAPTURE REPORT
```
