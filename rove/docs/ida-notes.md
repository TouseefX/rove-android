# IDA cross-check: rove vs. the real Mac client (RobloxPlayer 9.4-era ARM64)

Extracted artifacts live in `../rove-extract/` (233k symbols, 137 network
vtables, 83 packet item classes, full strings dump) and copied summaries in
`ida-packet-items.txt`, `ida-symbols.tsv`, `ida-vtables.json`.

## Transport mapping

| IDA class | rove crate | status |
|---|---|---|
| `RBX::RbxTransport::Impl::QuicMultiplexedConnection` | `client-core/src/transport` (quinn + wtransport, WebTransport-style) | matches — rove joins real places over QUIC |
| `RBX::Network::RakNetConnection` / `RakNetClientConnection` | not used on the live path (legacy UDP) | documented gap, sessions work without it |
| `RBX::Network::Peer/Client/Server` vtables | `Client`/`Server` handled implicitly by session layer | ok |

## Replication item coverage

All 83 `Replicator::*Item` / `Deserialized*Item` classes found in IDA are
covered by rove's decode path (`client-core/src/replication.rs`): world deltas,
property changes, attributes, tags, pings, join data, shared strings, ISR
timestamps, streaming focus/region items, GC removals, event invocations.

Observed live-session counters (`unreliable_channels_received`,
`world_packets_committed`) line up with the same `Replicator::ItemSender`
packet id/type/subtype logging strings found in the binary.

## ISR note

One known gap observed in a live session: `Unsupported ISR property type 10`
inside `instance_state_replication` decode — matches the
`InstanceStateSerializationManager` path in IDA. Non-fatal; shared world
frames still commit.

## Compression

IDA strings show `SNAPPY`, `Zstd`, `ZstdCompression`, zstd dictionaries;
rove uses zstd with bounded windows (`client-core/src/wire.rs`).

## VM

Official Luau (`vendor/luau`, Luau::CodeGen present in both) is embedded in
rust via `rust-runtime-sys`; IDA confirms the same VM version family
(`lua_State`, `luarequire_Configuration`, bytecode v14 seen live).

## RAM

`--lightweight` + mimalloc + 1 MiB cold-property disk cache: measured 26–31 MB
resident on a 5000-node live place. RAM budget strings in the binary
(`RAMBudgetBytes` style flags) correspond to rove's `ram_budget_bytes`.
