# The join: allocation, session, and the real thing

M2 has three halves, and they fail in different places — so they are built and
checked separately, and each one says which stage it reached.

| stage | what it is | what proves it | needs a server |
|---|---|---|---|
| **M2a** | the join-game response, parsed: the dial address, the port, the certificate pins, the token, the identity | `crates/rustudio-join/tests/allocation.rs` against recorded bodies | no |
| | the five login packets | `tests/login_packets.rs` against rove's Python vectors **and** rove's own encoder | no |
| **M2b** | a session fed a recorded stream through the same `Session` a live join uses | `tests/session_replay.rs` — the world it builds is compared to the one `rustudio-live`'s offline replay proved | no |
| **M2c** | a real join: sign in, allocate, dial, log in, receive | a phone, with the overlay on | yes |

## What the wire actually looks like

`POST gamejoin.roblox.com/v1/join-game` (or `/v1/join-game-instance` for a named
server) returns a `joinScript`, and the fields that decide where the client
dials are not where they used to be:

| what | where | fallback |
|---|---|---|
| the address to dial | `joinScript.UdmuxEndpoints[0].Address` | `MachineAddress` |
| the port | `NetStackConfig.port` (percent-encoded JSON) | `NetStackPort`, then `ServerPort` |
| the certificate pins | `NetStackConfig.certHashes` (base64 SHA-256) | none — no pins, no dial |
| the routing token | `NetStackTokenValue` | sixteen zero bytes when absent |
| the backend in the routing header | `MachineAddress` | — |

With udmux in front, the address to dial and the machine the header names are
**different addresses**. `tests/allocation.rs` asserts both, because getting them
from the same field is the mistake that produces a connection that is accepted
and then silently mis-routed.

Two of these are refusals rather than defaults on purpose: an address that is
not globally routable (loopback, private, link-local) is not dialled, and a
`NetStackConfig` whose port disagrees with `NetStackPort` beside it yields no
pins and no port, so the allocation is reported as not transport-ready rather
than dialled unpinned.

## The session

```
dial ──► control opens (applications 0, 4, 6) ──► login packets (4/1) ──► receives
                                                                          │
                                        SessionCore::ingest ──► journal ──┤
                                                                          ▼
                                                       rustudio_live::Live::apply
                                                                          │
                                                                     Document
```

`Session::feed` is the only path from bytes to world state, and `session::run`
calls it with exactly what its reliable stream delivered. That is why the
recording test is evidence for the live path and not a parallel implementation:
both go through `feed`, and `feed` is where rove's decoder, rove's journal and
rustudio's bridge meet.

A refused packet is counted and the session keeps reading. A hostile or broken
server must not be able to end a session by sending one bad packet, and a
refusal is data about the server.

## The five login packets

| # | packet | id | built from |
|---|---|---|---|
| 1 | early auth | `0xa8` | the two blobs in the issued `ClientTicket` |
| 2 | protocol sync | `0x90` | the retained join fields, the platform, six constants |
| 3 | verification | `0x92` | a fresh nonce from the OS |
| 4 | ticket | `0x8a` | the client ticket, its XXH32 and checksum, `SessionId` |
| 5 | server id | `0x8f` | nothing |

The bytes are checked three ways: against the Python reference vectors rove
ships, against rove's own Rust encoder compiled in as a dev-dependency, and
against each other (the vectors cover the shapes; the encoder covers the
branches).

## M2c: the part only a phone can answer

Nothing in CI can stand in for this, because it needs an account, a live server
and a certificate the server actually presents. What is built for it:

- `--features live`: `AccountSession` (sign in, the join-game POST, the
  anti-CSRF retry), and `session::run` over rove's own WebTransport client.
- `diagnostics::Report`: one line per stage — sign in, allocate, connect, log in,
  receive — with the failed one named and the duration of each. It carries no
  secret: the user id and the endpoint appear, the cookie, token and tickets do
  not.

A report that stops at **allocate** means the account or the request; one that
stops at **connect** means the pins or the address; one that reaches **log in**
and then closes means the server accepted the connection and rejected the
credentials. Those are three different bugs, and the overlay is what tells them
apart without a capture.
