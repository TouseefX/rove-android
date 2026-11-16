# sunc update

this is the current rust runtime snapshot, with the official luau vm.

- the engine bindings and host now run in rust by default.
- scripting includes the latest scheduler, script tracking, debug and signal work.
- world updates cover movement, physics, attributes, tags and property events.
- the terminal ui adds game search, a luau console, artwork and ram/ping graphs.
- the windows package includes the MIT license and dependency notices.

## checked locally

tested on macOS with rust 1.92.0:

- rust client library: 912 passed, 2 failed.
- core with transport and paged properties: 238 passed, 1 ignored.
- shared-payload integration test: passed after updating its expected attribute and tag fields.
- official luau vm boundary: 8 passed.
- terminal ui: 44 passed, 1 ignored.

the two runtime failures are still here:

- `rust_host::actors::tests::getconnections_exposes_real_foreign_actor_records_with_generation_fencing`
- `rust_host::world_tests::instance_tostring_uses_current_name_independent_of_parent_identity`

these are local checks, not a new live sunc score. windows is covered by the included github workflow; a successful windows run has not been verified for this update.

the optional decompiler needs the separately installed Tovek backend. this repo includes its MIT notice and pinned installation metadata, not the executable.
