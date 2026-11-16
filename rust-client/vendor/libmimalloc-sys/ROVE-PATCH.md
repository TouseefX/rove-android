# local allocator patch

Upstream: libmimalloc-sys 0.1.49, with its bundled mimalloc v2 and v3. Original MIT licenses are retained.

The decommit change in v2/src/prim/unix/prim.c: macOS decommit replaces a free page range with a PROT_NONE anonymous mapping at the same address. Mimalloc is told that the range must be recommitted before reuse. This activates the fixed-mapping approach shown in the upstream decommit implementation. Other platforms and v3 keep upstream behavior.

This releases physical backing immediately; it does not remove live allocations or change the engine memory cap. The fallback is the original macOS reusable-page advice if mapping fails.

On macOS, v2/src/options.c also defaults to 32 MiB arena reservations, lazy page commit, zero purge delays, purging free abandoned pages, and collecting free pages every 100 generic allocation calls. The more frequent collection releases unused pages between startup and steady state. Environment options still override these defaults. Other platforms retain the upstream settings.

build.rs explicitly watches itself and the selected C source/header tree. The upstream cc environment watches otherwise prevented changes in the files included by static.c from rebuilding the native archive.
