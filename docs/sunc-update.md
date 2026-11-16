# sunc update

this is the current rust runtime snapshot, with the official luau vm.

- the engine bindings and host now run in rust by default.
- scripting includes the latest scheduler, script tracking, debug and signal work.
- world updates cover movement, physics, attributes, tags and property events.
- the terminal ui adds game search, a luau console, artwork and ram/ping graphs.
- the windows package includes the MIT license and dependency notices.

## follow-up fixes

- `GetFullName()` leaves `game` out of descendant paths and keeps detached ancestors.
- the actor test now expects only the new VM's connection after replacement, and checks that old handles cannot change it.
- game scripts can use `FloatCurveKey` and `RotationCurveKey`, including inside actors.
- both command-line entry points use the same user agent. the VM bridge includes the C++ header required by MSVC.
- physics tests explicitly supply anchoring state and advance queued callbacks before checking completion.

## verification

the first regression fix passed all 914 macOS runtime tests on `3ca894c`, using rust 1.92.0. both command-line entry points also passed compilation after the shared-identity fix.

the [windows workflow](https://github.com/alexkkork/roblox-client/actions/workflows/rust-desktop.yml) checks the core, official VM, all client test targets, terminal frontend and real worker protocol. a separate job builds the release package and runs the packaged examples on windows. failed tests remain failures, and the full client suite reports all failing targets in one run.

these are offline build and runtime checks, not a new live sunc score or a live windows game-session test.

the optional decompiler needs the separately installed Tovek backend. this repo includes its MIT notice and pinned installation metadata, not the executable.
