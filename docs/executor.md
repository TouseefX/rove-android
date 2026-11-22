# rove executor

Headless Roblox executor built on rove's Rust client + the official Luau VM.

```sh
./bin/rove-exec 1818 my_script.luau 20
```

- authenticates with `secrets/roblox-cookie.txt` (`.ROBLOSECURITY`, chmod 600, dir 700)
- joins the place over the real QUIC transport (Roblox's RbxTransport path)
- replicates the world with rove's decoder (83 replication item kinds)
- runs your Luau against the received DataModel with the official VM
- `--lightweight` keeps resident RAM at ~20-45 MB even in 40k+ node places
- `ROVE_TOLERATE_UNKNOWN_SETS=1` (set by the wrapper) keeps sessions alive when
  a rare packet is skipped; the world still replicates normally

Notes:
- scripts run in the client VM; `game`, services, instances, remotes, signals,
  scheduler and physics reads are available
- server-side anti-cheat that rejects non-standard clients can still block joins
- `rove auth` verifies the cookie; `rove join --physics` enables own-character movement

## Session results (Nov 21, 2026)

| place | nodes replicated | RAM (resident) | peak | spawned | notes |
|---|---|---|---|---|---|
| 1818 Classic Crossroads | 5,575 | 40.6 MB | 73 MB | yes | full replication, physics-ready |
| 920587237 Adopt Me! | 42,117 | 40.6 MB | 215 MB | join ok | lightweight mode, 7 rare packets skipped |

Both joins authenticated as a real account over the QUIC transport and ran
authored Luau through the official VM embedded in the Rust client.

### Skip-rate after zstd list fix

- Adopt Me!: 1,105 committed / 7 rejected (0.6%), ISR decode 50/0,
  zero engine errors, session stable, 39.5 MB RAM.
- Crossroads: 0 skips, full spawn.

### 3-minute soak (Adopt Me!)

- 2,468 packets committed / 7 rejected (stable), 42,198 nodes
- RSS stayed flat at ~55-57 MB for the full session (mimalloc + lightweight)
- Script executed cleanly; zero engine errors; transport_error: none

### Phantom Forces (big FPS)

- 89,330 nodes replicated, 2,849 packets committed / 0 rejected
- 35.2 MB resident RAM (lightweight) — heavy FPS world under 36 MB
- Scripts executed; session clean end.
